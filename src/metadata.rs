use chrono::DateTime;
use chrono::offset::Utc;
use data_encoding::HEXLOWER;
use json;
use pem;
use ring;
use ring::digest::{digest, SHA256};
use ring::signature::{ED25519, RSA_PSS_2048_8192_SHA256, RSA_PSS_2048_8192_SHA512};
use serde::de::{Deserialize, DeserializeOwned, Deserializer, Error as DeserializeError};
use std::collections::HashMap;
use std::fmt::{self, Display, Formatter, Debug};
use std::marker::PhantomData;
use std::str::FromStr;
use untrusted::Input;

use cjson::canonicalize;
use error::Error;
use rsa::convert_to_pkcs1;

static HASH_PREFERENCES: &'static [HashType] = &[HashType::Sha512, HashType::Sha256];

#[derive(Eq, PartialEq, Deserialize, Debug, Clone)]
pub enum Role {
    Root,
    Targets,
    Timestamp,
    Snapshot,
    TargetsDelegation(String),
}

impl FromStr for Role {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Root" => Ok(Role::Root),
            "Snapshot" => Ok(Role::Snapshot),
            "Targets" => Ok(Role::Targets),
            "Timestamp" => Ok(Role::Timestamp),
            role => Err(Error::UnknownRole(String::from(role))),
        }
    }
}

impl Display for Role {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match *self {
            Role::Root => write!(f, "{}", "root"),
            Role::Targets => write!(f, "{}", "targets"),
            Role::Snapshot => write!(f, "{}", "snapshot"),
            Role::Timestamp => write!(f, "{}", "timestamp"),
            Role::TargetsDelegation(ref s) => write!(f, "{}", s),
        }
    }
}

pub trait RoleType: Debug + Clone{
    fn matches(role: &Role) -> bool;
}

#[derive(Debug, Clone)]
pub struct Root {}
impl RoleType for Root {
    fn matches(role: &Role) -> bool {
        match role {
            &Role::Root => true,
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Targets {}
impl RoleType for Targets {
    fn matches(role: &Role) -> bool {
        match role {
            &Role::Targets => true,
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Timestamp {}
impl RoleType for Timestamp {
    fn matches(role: &Role) -> bool {
        match role {
            &Role::Timestamp => true,
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Snapshot {}
impl RoleType for Snapshot {
    fn matches(role: &Role) -> bool {
        match role {
            &Role::Snapshot => true,
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SignedMetadata<R: RoleType + Clone> {
    pub signatures: Vec<Signature>,
    pub signed: json::Value,
    _role: PhantomData<R>,
}

impl<'de, R: RoleType> Deserialize<'de> for SignedMetadata<R> {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        if let json::Value::Object(mut object) = Deserialize::deserialize(de)? {
            match (object.remove("signatures"), object.remove("signed")) {
                (Some(a @ json::Value::Array(_)), Some(v @ json::Value::Object(_))) => {
                    Ok(SignedMetadata::<R> {
                        signatures: json::from_value(a).map_err(|e| {
                                DeserializeError::custom(format!("Bad signature data: {}", e))
                            })?,
                        signed: v.clone(),
                        _role: PhantomData,
                    })
                }
                _ => {
                    Err(DeserializeError::custom("Metadata missing 'signed' or 'signatures' \
                                                  section"))
                }
            }
        } else {
            Err(DeserializeError::custom("Metadata was not an object"))
        }
    }
}

pub trait Metadata<R: RoleType>: DeserializeOwned {
    fn expires(&self) -> &DateTime<Utc>;
}


#[derive(Debug, PartialEq)]
pub struct RootMetadata {
    consistent_snapshot: bool,
    expires: DateTime<Utc>,
    pub version: i32,
    pub keys: HashMap<KeyId, Key>,
    pub root: RoleDefinition,
    pub targets: RoleDefinition,
    pub timestamp: RoleDefinition,
    pub snapshot: RoleDefinition,
}

impl Metadata<Root> for RootMetadata {
    fn expires(&self) -> &DateTime<Utc> {
        &self.expires
    }
}

impl<'de> Deserialize<'de> for RootMetadata {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        if let json::Value::Object(mut object) = Deserialize::deserialize(de)? {
            let typ = json::from_value::<Role>(object.remove("_type")
                    .ok_or_else(|| DeserializeError::custom("Field '_type' missing"))?)
                .map_err(|e| {
                    DeserializeError::custom(format!("Field '_type' not a valid role: {}", e))
                })?;

            if typ != Role::Root {
                return Err(DeserializeError::custom("Field '_type' was not 'Root'"));
            }

            let keys = json::from_value(object.remove("keys")
                    .ok_or_else(|| DeserializeError::custom("Field 'keys' missing"))?).map_err(|e| {
                    DeserializeError::custom(format!("Field 'keys' not a valid key map: {}", e))
                })?;

            let expires = json::from_value(object.remove("expires")
                    .ok_or_else(|| DeserializeError::custom("Field 'expires' missing"))?).map_err(|e| {
                    DeserializeError::custom(format!("Field 'expires' did not have a valid format: {}", e))
                })?;

            let version = json::from_value(object.remove("version")
                    .ok_or_else(|| DeserializeError::custom("Field 'version' missing"))?).map_err(|e| {
                    DeserializeError::custom(format!("Field 'version' did not have a valid format: {}", e))
                })?;

            let consistent_snapshot = json::from_value(object.remove("consistent_snapshot")
                    .ok_or_else(|| DeserializeError::custom("Field 'consistent_snapshot' missing"))?).map_err(|e| {
                    DeserializeError::custom(format!("Field 'consistent_snapshot' did not have a valid format: {}", e))
                })?;

            let mut roles = object.remove("roles")
                .and_then(|v| match v {
                    json::Value::Object(o) => Some(o),
                    _ => None,
                })
                .ok_or_else(|| DeserializeError::custom("Field 'roles' missing"))?;

            let root = json::from_value(roles.remove("root")
                    .ok_or_else(|| DeserializeError::custom("Role 'root' missing"))?)
                .map_err(|e| {
                    DeserializeError::custom(format!("Root role definition error: {}", e))
                })?;

            let targets = json::from_value(roles.remove("targets")
                    .ok_or_else(|| DeserializeError::custom("Role 'targets' missing"))?)
                .map_err(|e| {
                    DeserializeError::custom(format!("Targets role definition error: {}", e))
                })?;

            let timestamp = json::from_value(roles.remove("timestamp")
                    .ok_or_else(|| DeserializeError::custom("Role 'timestamp' missing"))?)
                .map_err(|e| {
                    DeserializeError::custom(format!("Timetamp role definition error: {}", e))
                })?;

            let snapshot = json::from_value(roles.remove("snapshot")
                    .ok_or_else(|| DeserializeError::custom("Role 'shapshot' missing"))?)
                .map_err(|e| {
                    DeserializeError::custom(format!("Snapshot role definition error: {}", e))
                })?;

            Ok(RootMetadata {
                consistent_snapshot,
                expires: expires,
                version: version,
                keys: keys,
                root: root,
                targets: targets,
                timestamp: timestamp,
                snapshot: snapshot,
            })
        } else {
            Err(DeserializeError::custom("Role was not an object"))
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct RoleDefinition {
    pub key_ids: Vec<KeyId>,
    pub threshold: i32,
}

impl<'de> Deserialize<'de> for RoleDefinition {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        if let json::Value::Object(mut object) = Deserialize::deserialize(de)? {
            let key_ids = json::from_value(object.remove("keyids")
                    .ok_or_else(|| DeserializeError::custom("Field 'keyids' missing"))?).map_err(|e| {
                    DeserializeError::custom(format!("Field 'keyids' not a valid array: {}", e))
                })?;

            let threshold = json::from_value(object.remove("threshold")
                    .ok_or_else(|| DeserializeError::custom("Field 'threshold' missing"))?).map_err(|e| {
                    DeserializeError::custom(format!("Field 'threshold' not a an int: {}", e))
                })?;

            if threshold <= 0 {
                return Err(DeserializeError::custom("'threshold' must be >= 1"));
            }


            Ok(RoleDefinition {
                key_ids: key_ids,
                threshold: threshold,
            })
        } else {
            Err(DeserializeError::custom("Role definition was not an object"))
        }
    }
}

#[derive(Debug, Clone)]
pub struct TargetsMetadata {
    expires: DateTime<Utc>,
    pub version: i32,
    pub delegations: Option<Delegations>,
    pub targets: HashMap<String, TargetInfo>,
}

impl Metadata<Targets> for TargetsMetadata {
    fn expires(&self) -> &DateTime<Utc> {
        &self.expires
    }
}

impl<'de> Deserialize<'de> for TargetsMetadata {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        if let json::Value::Object(mut object) = Deserialize::deserialize(de)? {
            let delegations = match object.remove("delegations") {
                // TODO this should accept null / empty object too
                // currently the options are "not present at all" or "completely correct"
                // and everything else errors out
                Some(value) => {
                    Some(json::from_value(value).map_err(|e| {
                            DeserializeError::custom(format!("Bad delegations format: {}", e))
                        })?)
                }
                None => None,
            };

            let expires = json::from_value(object.remove("expires")
                    .ok_or_else(|| DeserializeError::custom("Field 'expires' missing"))?).map_err(|e| {
                    DeserializeError::custom(format!("Field 'expires did not have a valid format: {}", e))
                })?;

            let version = json::from_value(object.remove("version")
                    .ok_or_else(|| DeserializeError::custom("Field 'version' missing"))?).map_err(|e| {
                    DeserializeError::custom(format!("Field 'version' did not have a valid format: {}", e))
                })?;

            match object.remove("targets") {
                Some(t) => {
                    let targets =
                        json::from_value(t).map_err(|e| {
                                DeserializeError::custom(format!("Bad targets format: {}", e))
                            })?;

                    Ok(TargetsMetadata {
                        version: version,
                        expires: expires,
                        delegations: delegations,
                        targets: targets,
                    })
                }
                _ => Err(DeserializeError::custom("Signature missing fields".to_string())),
            }
        } else {
            Err(DeserializeError::custom("Role was not an object"))
        }
    }
}


#[derive(Debug)]
pub struct TimestampMetadata {
    expires: DateTime<Utc>,
    pub version: i32,
    pub meta: HashMap<String, MetadataMetadata>,
}

impl Metadata<Timestamp> for TimestampMetadata {
    fn expires(&self) -> &DateTime<Utc> {
        &self.expires
    }
}

impl<'de> Deserialize<'de> for TimestampMetadata {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        if let json::Value::Object(mut object) = Deserialize::deserialize(de)? {

            let expires = json::from_value(object.remove("expires")
                    .ok_or_else(|| DeserializeError::custom("Field 'expires' missing"))?).map_err(|e| {
                    DeserializeError::custom(format!("Field 'expires' did not have a valid format: {}", e))
                })?;

            let version = json::from_value(object.remove("version")
                    .ok_or_else(|| DeserializeError::custom("Field 'version' missing"))?).map_err(|e| {
                    DeserializeError::custom(format!("Field 'version' did not have a valid format: {}", e))
                })?;

            match object.remove("meta") {
                Some(m) => {
                    let meta = json::from_value(m).map_err(|e| {
                            DeserializeError::custom(format!("Bad meta-meta format: {}", e))
                        })?;

                    Ok(TimestampMetadata {
                        expires: expires,
                        version: version,
                        meta: meta,
                    })
                }
                _ => Err(DeserializeError::custom("Signature missing fields".to_string())),
            }
        } else {
            Err(DeserializeError::custom("Role was not an object"))
        }
    }
}


#[derive(Debug)]
pub struct SnapshotMetadata {
    expires: DateTime<Utc>,
    pub version: i32,
    pub meta: HashMap<String, SnapshotMetadataMetadata>,
}

impl Metadata<Snapshot> for SnapshotMetadata {
    fn expires(&self) -> &DateTime<Utc> {
        &self.expires
    }
}

impl<'de> Deserialize<'de> for SnapshotMetadata {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        if let json::Value::Object(mut object) = Deserialize::deserialize(de)? {
            let expires = json::from_value(object.remove("expires")
                    .ok_or_else(|| DeserializeError::custom("Field 'expires' missing"))?).map_err(|e| {
                    DeserializeError::custom(format!("Field 'expires' did not have a valid format: {}", e))
                })?;

            let version = json::from_value(object.remove("version")
                    .ok_or_else(|| DeserializeError::custom("Field 'version' missing"))?).map_err(|e| {
                    DeserializeError::custom(format!("Field 'version' did not have a valid format: {}", e))
                })?;

            match object.remove("meta") {
                Some(m) => {
                    let meta = json::from_value(m).map_err(|e| {
                            DeserializeError::custom(format!("Bad meta-meta format: {}", e))
                        })?;

                    Ok(SnapshotMetadata {
                        expires: expires,
                        version: version,
                        meta: meta,
                    })
                }
                _ => Err(DeserializeError::custom("Signature missing fields".to_string())),
            }
        } else {
            Err(DeserializeError::custom("Role was not an object"))
        }
    }
}

/// A cryptographic signature.
#[derive(Clone, PartialEq, Debug)]
pub struct Signature {
    pub key_id: KeyId,
    pub method: SignatureScheme,
    pub sig: SignatureValue,
}

impl<'de> Deserialize<'de> for Signature {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        if let json::Value::Object(mut object) = Deserialize::deserialize(de)? {
            match (object.remove("keyid"), object.remove("method"), object.remove("sig")) {
                (Some(k), Some(m), Some(s)) => {
                    let key_id =
                        json::from_value(k).map_err(|e| {
                                DeserializeError::custom(format!("Failed at keyid: {}", e))
                            })?;
                    let method =
                        json::from_value(m).map_err(|e| {
                                DeserializeError::custom(format!("Failed at method: {}", e))
                            })?;
                    let sig = json::from_value(s)
                        .map_err(|e| DeserializeError::custom(format!("Failed at sig: {}", e)))?;

                    Ok(Signature {
                        key_id: key_id,
                        method: method,
                        sig: sig,
                    })
                }
                _ => Err(DeserializeError::custom("Signature missing fields".to_string())),
            }
        } else {
            Err(DeserializeError::custom("Signature was not an object".to_string()))
        }
    }
}


/// A public key
#[derive(Clone, PartialEq, Debug, Deserialize)]
pub struct Key {
    /// The type of keys.
    #[serde(rename = "keytype")]
    pub typ: KeyType,
    /// The key's value.
    #[serde(rename = "keyval")]
    pub value: KeyValue,
}

impl Key {
    /// Use the given key to verify a signature over a byte array.
    pub fn verify(&self,
                  scheme: &SignatureScheme,
                  msg: &[u8],
                  sig: &SignatureValue)
                  -> Result<(), Error> {
        if self.typ.supports(scheme) {
            match self.typ {
                KeyType::Unsupported(ref s) => Err(Error::UnsupportedKeyType(s.clone())),
                _ => scheme.verify(&self.value, msg, sig),
            }
        } else {
            Err(Error::Generic(format!("Signature scheme mismatch: Key {:?}, Scheme {:?}",
                                       self,
                                       scheme)))
        }
    }
}

/// Types of public keys.
#[derive(Clone, PartialEq, Debug)]
pub enum KeyType {
    /// [Ed25519](https://en.wikipedia.org/wiki/EdDSA#Ed25519) signature scheme.
    Ed25519,
    /// [RSA](https://en.wikipedia.org/wiki/RSA_%28cryptosystem%29)
    Rsa,
    /// Internal representation of an unsupported key type.
    Unsupported(String),
}

impl KeyType {
    fn supports(&self, scheme: &SignatureScheme) -> bool {
        match (self, scheme) {
            (&KeyType::Ed25519, &SignatureScheme::Ed25519) => true,
            (&KeyType::Rsa, &SignatureScheme::RsaSsaPssSha256) => true,
            (&KeyType::Rsa, &SignatureScheme::RsaSsaPssSha512) => true,
            _ => false,
        }
    }
}

impl FromStr for KeyType {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "ed25519" => Ok(KeyType::Ed25519),
            "rsa" => Ok(KeyType::Rsa),
            typ => Ok(KeyType::Unsupported(typ.into())),
        }
    }
}

impl<'de> Deserialize<'de> for KeyType {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        if let json::Value::String(ref s) = Deserialize::deserialize(de)? {
            s.parse().map_err(|_| unreachable!())
        } else {
            Err(DeserializeError::custom("Key type was not a string"))
        }
    }
}


/// The raw bytes of a public key.
#[derive(Clone, PartialEq, Debug)]
pub struct KeyValue {
    /// The key's raw bytes.
    pub value: Vec<u8>,
    /// The key's original value, needed for ID calculation
    pub original: String,
    /// The key's type,
    pub typ: KeyType,
}

impl KeyValue {
    /// Calculates the `KeyId` of the public key.
    pub fn key_id(&self) -> KeyId {
        match self.typ {
            KeyType::Unsupported(_) => KeyId(String::from("error")), // TODO this feels wrong, but we check this everywhere else
            _ => {
                let key_value = canonicalize(&json::Value::String(self.original.clone())).unwrap(); // TODO unwrap
                KeyId(HEXLOWER.encode(digest(&SHA256, &key_value).as_ref()))
            }
        }
    }
}

impl<'de> Deserialize<'de> for KeyValue {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        match Deserialize::deserialize(de)? {
            json::Value::String(ref s) => {
                // TODO this is pretty shaky
                if s.starts_with("-----") {
                    pem::parse(s)
                        .map(|p| {
                            KeyValue {
                                value: p.contents,
                                original: s.clone(),
                                typ: KeyType::Rsa,
                            }
                        })
                        .map_err(|e| {
                            DeserializeError::custom(format!("Key was not PEM encoded: {}", e))
                        })
                } else {
                    HEXLOWER.decode(s.as_ref())
                        .map(|v| {
                            KeyValue {
                                value: v,
                                original: s.clone(),
                                typ: KeyType::Ed25519,
                            }
                        })
                        .map_err(|e| {
                            DeserializeError::custom(format!("Key value was not hex: {}", e))
                        })
                }
            }
            json::Value::Object(mut object) => {
                json::from_value::<KeyValue>(object.remove("public")
                        .ok_or_else(|| DeserializeError::custom("Field 'public' missing"))?)
                    .map_err(|e| {
                        DeserializeError::custom(format!("Field 'public' not encoded correctly: \
                                                          {}",
                                                         e))
                    })
            }
            _ => Err(DeserializeError::custom("Key value was not a string or object")),
        }
    }
}


/// The hex encoded ID of a public key.
#[derive(Clone, Hash, Eq, PartialEq, Debug)]
pub struct KeyId(pub String);

impl<'de> Deserialize<'de> for KeyId {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        match Deserialize::deserialize(de)? {
            json::Value::String(s) => Ok(KeyId(s)),
            _ => Err(DeserializeError::custom("Key ID was not a string")),
        }
    }
}


#[derive(Clone, PartialEq, Debug)]
pub struct SignatureValue(Vec<u8>);

impl<'de> Deserialize<'de> for SignatureValue {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        match Deserialize::deserialize(de)? {
            json::Value::String(ref s) => {
                HEXLOWER.decode(s.as_ref())
                    .map(SignatureValue)
                    .map_err(|e| {
                        DeserializeError::custom(format!("Signature value was not hex: {}", e))
                    })
            }
            _ => Err(DeserializeError::custom("Signature value was not a string")),
        }
    }
}


#[derive(Clone, PartialEq, Debug)]
pub enum SignatureScheme {
    Ed25519,
    RsaSsaPssSha256,
    RsaSsaPssSha512,
    Unsupported(String),
}

impl SignatureScheme {
    fn verify(&self, pub_key: &KeyValue, msg: &[u8], sig: &SignatureValue) -> Result<(), Error> {
        let alg: &ring::signature::VerificationAlgorithm = match self {
            &SignatureScheme::Ed25519 => &ED25519,
            &SignatureScheme::RsaSsaPssSha256 => &RSA_PSS_2048_8192_SHA256,
            &SignatureScheme::RsaSsaPssSha512 => &RSA_PSS_2048_8192_SHA512,
            &SignatureScheme::Unsupported(ref s) => {
                return Err(Error::UnsupportedSignatureScheme(s.clone()));
            }
        };

        ring::signature::verify(alg, Input::from(&convert_to_pkcs1(&pub_key.value)),
                                Input::from(msg), Input::from(&sig.0))
            .map_err(|_| Error::VerificationFailure("Bad signature".into()))
    }
}

impl FromStr for SignatureScheme {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "ed25519" => Ok(SignatureScheme::Ed25519),
            "rsassa-pss-sha256" => Ok(SignatureScheme::RsaSsaPssSha256),
            "rsassa-pss-sha512" => Ok(SignatureScheme::RsaSsaPssSha512),
            typ => Ok(SignatureScheme::Unsupported(typ.into())),
        }
    }
}

impl<'de> Deserialize<'de> for SignatureScheme {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        if let json::Value::String(ref s) = Deserialize::deserialize(de)? {
            s.parse().map_err(|_| unreachable!())
        } else {
            Err(DeserializeError::custom("Key type was not a string"))
        }
    }
}


#[derive(Clone, PartialEq, Debug, Deserialize)]
pub struct MetadataMetadata {
    pub length: i64,
    pub hashes: HashMap<HashType, HashValue>,
    pub version: i32,
}


#[derive(Clone, PartialEq, Debug, Deserialize)]
pub struct SnapshotMetadataMetadata {
    pub length: Option<i64>,
    pub hashes: Option<HashMap<HashType, HashValue>>,
    pub version: i32,
}


#[derive(Clone, Hash, Eq, PartialEq, Debug)]
pub enum HashType {
    Sha256,
    Sha512,
    Unsupported(String),
}

impl HashType {
    pub fn preferences() -> &'static [HashType] {
        HASH_PREFERENCES
    }
}

impl FromStr for HashType {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "sha256" => Ok(HashType::Sha256),
            "sha512" => Ok(HashType::Sha512),
            typ => Ok(HashType::Unsupported(typ.into())),
        }
    }
}

impl<'de> Deserialize<'de> for HashType {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        if let json::Value::String(ref s) = Deserialize::deserialize(de)? {
            s.parse().map_err(|_| unreachable!())
        } else {
            Err(DeserializeError::custom("Hash type was not a string"))
        }
    }
}


#[derive(Clone, PartialEq, Debug)]
pub struct HashValue(pub Vec<u8>);
impl<'de> Deserialize<'de> for HashValue {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        match Deserialize::deserialize(de)? {
            json::Value::String(ref s) => {
                HEXLOWER.decode(s.as_ref())
                    .map(HashValue)
                    .map_err(|e| DeserializeError::custom(format!("Hash value was not hex: {}", e)))
            }
            _ => Err(DeserializeError::custom("Hash value was not a string")),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct TargetInfo {
    pub length: i64,
    pub hashes: HashMap<HashType, HashValue>,
    pub custom: Option<HashMap<String, json::Value>>,
}


#[derive(Clone, PartialEq, Debug, Deserialize)]
pub struct Delegations {
    pub keys: HashMap<KeyId, Key>,
    pub roles: Vec<DelegatedRole>,
}


#[derive(Clone, PartialEq, Debug)]
pub struct DelegatedRole {
    pub name: String,
    pub key_ids: Vec<KeyId>,
    pub threshold: i32,
    pub terminating: bool,
    paths: TargetPaths,
}

impl DelegatedRole {
    pub fn could_have_target(&self, target: &str) -> bool {
        match self.paths {
            TargetPaths::Patterns(ref patterns) => {
                for path in patterns.iter() {
                    let path_str = path.as_str();
                    if path_str == target {
                        return true
                    } else if path_str.ends_with("/") && target.starts_with(path_str) {
                         return true
                    }
                }
                return false
            }
        }
    }
}

impl<'de> Deserialize<'de> for DelegatedRole {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        if let json::Value::Object(mut object) = Deserialize::deserialize(de)? {
            match (object.remove("name"), object.remove("keyids"),
                   object.remove("threshold"), object.remove("terminating"),
                   object.remove("paths"), object.remove("path_hash_prefixes")) {
                (Some(n), Some(ks), Some(t), Some(term), Some(ps), None) => {
                    let name =
                        json::from_value(n).map_err(|e| {
                                DeserializeError::custom(format!("Failed at name: {}", e))
                            })?;

                    let key_ids =
                        json::from_value(ks).map_err(|e| {
                                DeserializeError::custom(format!("Failed at keyids: {}", e))
                            })?;

                    let threshold =
                        json::from_value(t).map_err(|e| {
                                DeserializeError::custom(format!("Failed at treshold: {}", e))
                            })?;

                    let terminating =
                        json::from_value(term).map_err(|e| {
                                DeserializeError::custom(format!("Failed at treshold: {}", e))
                            })?;

                    let paths: Vec<String> =
                        json::from_value(ps).map_err(|e| {
                                DeserializeError::custom(format!("Failed at treshold: {}", e))
                            })?;

                    Ok(DelegatedRole {
                        name: name,
                        key_ids: key_ids,
                        threshold: threshold,
                        terminating: terminating,
                        paths: TargetPaths::Patterns(paths),
                    })
                }
                (_, _, _, _, Some(_), Some(_)) =>
                    Err(DeserializeError::custom("Fields 'paths' or 'pash_hash_prefixes' are mutually exclusive".to_string())),
                (_, _, _, _, _, Some(_)) =>
                    Err(DeserializeError::custom("'pash_hash_prefixes' is not yet supported".to_string())),
                _ => Err(DeserializeError::custom("Signature missing fields".to_string())),
            }
        } else {
            Err(DeserializeError::custom("Delegated role was not an object".to_string()))
        }
    }
}


#[derive(Clone, PartialEq, Debug)]
pub enum TargetPaths {
    Patterns(Vec<String>),
    // TODO HashPrefixes(Vec<String>),
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn delegated_role_could_have_target() {
        let vectors = vec![
            ("foo", "foo", true),
            ("foo/", "foo/bar", true),
            ("foo", "foo/bar", false),
            ("foo/bar", "foo/baz", false),
            ("foo/bar/", "foo/bar/baz", true),
        ];

        for &(prefix, target, success) in vectors.iter() {
            let delegation = DelegatedRole {
                name: "".to_string(),
                key_ids: Vec::new(),
                threshold: 1,
                terminating: false,
                paths: TargetPaths::Patterns(vec![prefix.to_string()]),
            };

            assert!(!success ^ delegation.could_have_target(target),
                    format!("Prefix {} should have target {}: {}", prefix, target, success))
        };
    }
}
