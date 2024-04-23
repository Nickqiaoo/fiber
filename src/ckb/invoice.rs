#![allow(dead_code)]
use super::{
    gen::invoice::{self as gen_invoice, *},
    utils::{construct_invoice_preimage, BytesToBase32},
};
use crate::ckb::utils::{ar_decompress, ar_encompress};
use bech32::{encode, u5, FromBase32, ToBase32, Variant, WriteBase32};
use bitcoin::hashes::{sha256, Hash};

use bitcoin::secp256k1::{
    ecdsa::{RecoverableSignature, RecoveryId},
    Message, PublicKey,
};
use ckb_types::{
    packed::Script,
    prelude::{Pack, Unpack},
};
use core::time::Duration;
use molecule::prelude::{Builder, Entity};
use nom::{branch::alt, combinator::opt};
use nom::{
    bytes::{complete::take_while1, streaming::tag},
    IResult,
};
use serde::{Deserialize, Serialize};
use std::{cmp::Ordering, num::ParseIntError, str::FromStr};
use thiserror::Error;

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub enum Currency {
    Ckb,
    CkbTestNet,
}

impl From<u8> for Currency {
    fn from(byte: u8) -> Self {
        match byte {
            0 => Self::Ckb,
            1 => Self::CkbTestNet,
            _ => panic!("Invalid value for Currency"),
        }
    }
}

impl ToString for Currency {
    fn to_string(&self) -> String {
        match self {
            Currency::Ckb => "ckb".to_string(),
            Currency::CkbTestNet => "ckt".to_string(),
        }
    }
}

impl FromStr for Currency {
    type Err = InvoiceParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "ckb" => Ok(Self::Ckb),
            "ckt" => Ok(Self::CkbTestNet),
            _ => Err(InvoiceParseError::UnknownCurrency),
        }
    }
}

#[derive(Eq, PartialEq, Debug, Clone, Copy, Ord, PartialOrd, Serialize, Deserialize)]
pub enum SiPrefix {
    /// 10^-3
    Milli,
    /// 10^-6
    Micro,
    /// 10^3
    Kilo,
}

impl ToString for SiPrefix {
    fn to_string(&self) -> String {
        match self {
            SiPrefix::Milli => "m".to_string(),
            SiPrefix::Micro => "u".to_string(),
            SiPrefix::Kilo => "k".to_string(),
        }
    }
}

impl From<u8> for SiPrefix {
    fn from(byte: u8) -> Self {
        match byte {
            0 => Self::Milli,
            1 => Self::Micro,
            2 => Self::Kilo,
            _ => panic!("Invalid value for SiPrefix"),
        }
    }
}

impl FromStr for SiPrefix {
    type Err = InvoiceParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "m" => Ok(Self::Milli),
            "u" => Ok(Self::Micro),
            "k" => Ok(Self::Kilo),
            _ => Err(InvoiceParseError::UnknownSiPrefix),
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Attribute {
    FinalHtlcTimeout(u64),
    FinalHtlcMinimumCltvExpiry(u64),
    ExpiryTime(Duration),
    Description(String),
    FallbackAddr(String),
    UdtScript(Script),
    PayeePublicKey(PublicKey),
    Feature(u64),
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct InvoiceData {
    pub payment_hash: [u8; 32],
    pub payment_secret: [u8; 32],
    pub attrs: Vec<Attribute>,
}

/// Represents a syntactically and semantically correct lightning BOLT11 invoice.
///
/// There are three ways to construct a `CkbInvoice`:
///  1. using [`CkbInvoiceBuilder`]
///  2. using `str::parse::<CkbInvoice>(&str)` (see [`CkbInvoice::from_str`])
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct CkbInvoice {
    pub currency: Currency,
    pub amount: Option<u64>,
    pub prefix: Option<SiPrefix>,
    pub signature: Option<InvoiceSignature>,
    pub data: InvoiceData,
}

/// Recoverable signature
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvoiceSignature(pub RecoverableSignature);

impl PartialOrd for InvoiceSignature {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for InvoiceSignature {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0
            .serialize_compact()
            .1
            .cmp(&other.0.serialize_compact().1)
    }
}

impl CkbInvoice {
    fn hrp_part(&self) -> String {
        format!(
            "ln{}{}{}",
            self.currency.to_string(),
            self.amount
                .map_or_else(|| "".to_string(), |x| x.to_string()),
            self.prefix
                .map_or_else(|| "".to_string(), |x| x.to_string()),
        )
    }

    fn data_part(&self) -> Vec<u5> {
        let invoice_data = RawInvoiceData::from(self.data.clone());
        let compressed = ar_encompress(invoice_data.as_slice()).unwrap();
        let mut base32 = Vec::with_capacity(compressed.len());
        compressed.write_base32(&mut base32).unwrap();
        base32
    }

    fn is_signed(&self) -> bool {
        self.signature.is_some()
    }

    fn build_signature<F>(&mut self, sign_function: F) -> Result<(), SignOrCreationError>
    where
        F: FnOnce(&Message) -> RecoverableSignature,
    {
        let hrp = self.hrp_part();
        let data = self.data_part();
        let preimage = construct_invoice_preimage(hrp.as_bytes(), &data);
        let mut hash: [u8; 32] = Default::default();
        hash.copy_from_slice(&sha256::Hash::hash(&preimage)[..]);
        let message = Message::from_slice(&hash).unwrap();
        let signature = sign_function(&message);
        self.signature = Some(InvoiceSignature(signature));
        Ok(())
    }
}

impl ToBase32 for InvoiceSignature {
    fn write_base32<W: WriteBase32>(&self, writer: &mut W) -> Result<(), <W as WriteBase32>::Err> {
        let mut converter = BytesToBase32::new(writer);
        let (recovery_id, signature) = self.0.serialize_compact();
        converter.append(&signature[..])?;
        converter.append_u8(recovery_id.to_i32() as u8)?;
        converter.finalize()
    }
}

impl InvoiceSignature {
    fn from_base32(signature: &[u5]) -> Result<Self, InvoiceParseError> {
        if signature.len() != 104 {
            return Err(InvoiceParseError::InvalidSliceLength(
                "InvoiceSignature::from_base32()".into(),
            ));
        }
        let recoverable_signature_bytes = Vec::<u8>::from_base32(signature).unwrap();
        let signature = &recoverable_signature_bytes[0..64];
        let recovery_id = RecoveryId::from_i32(recoverable_signature_bytes[64] as i32).unwrap();

        Ok(InvoiceSignature(
            RecoverableSignature::from_compact(signature, recovery_id).unwrap(),
        ))
    }
}

impl ToString for CkbInvoice {
    fn to_string(&self) -> String {
        let hrp = self.hrp_part();
        let mut data = self.data_part();
        data.insert(
            0,
            u5::try_from_u8(if self.signature.is_some() { 1 } else { 0 }).unwrap(),
        );
        if let Some(signature) = &self.signature {
            data.extend_from_slice(&signature.to_base32());
        }
        encode(&hrp, data, Variant::Bech32m).unwrap()
    }
}

impl FromStr for CkbInvoice {
    type Err = InvoiceParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (hrp, data, var) = bech32::decode(s).unwrap();

        if var == bech32::Variant::Bech32 {
            return Err(InvoiceParseError::Bech32Error(
                bech32::Error::InvalidChecksum,
            ));
        }

        if data.len() < 104 {
            return Err(InvoiceParseError::TooShortDataPart);
        }
        let (currency, amount, prefix) = parse_hrp(&hrp)?;
        let is_signed = u5::from(data[0]).to_u8() == 1;
        let data_end = if is_signed {
            data.len() - 104
        } else {
            data.len()
        };
        let data_part = Vec::<u8>::from_base32(&data[1..data_end]).unwrap();
        let data_part = ar_decompress(&data_part).unwrap();
        let invoice_data = RawInvoiceData::from_slice(&data_part).unwrap();
        let signature = if is_signed {
            Some(InvoiceSignature::from_base32(&data[data.len() - 104..])?)
        } else {
            None
        };

        let invoice = CkbInvoice {
            currency,
            amount,
            prefix,
            signature,
            data: invoice_data.try_into().unwrap(),
        };
        Ok(invoice)
    }
}

#[derive(PartialEq, Eq, Debug, Clone)]
pub enum InvoiceParseError {
    Bech32Error(bech32::Error),
    ParseAmountError(ParseIntError),
    //MalformedSignature(secp256k1::Error),
    BadPrefix,
    UnknownCurrency,
    UnknownSiPrefix,
    MalformedHRP,
    TooShortDataPart,
    UnexpectedEndOfTaggedFields,
    PaddingError,
    IntegerOverflowError,
    InvalidSegWitProgramLength,
    InvalidPubKeyHashLength,
    InvalidScriptHashLength,
    InvalidRecoveryId,
    InvalidSliceLength(String),
    /// according to BOLT11
    Skip,
}

fn nom_scan_hrp(input: &str) -> IResult<&str, (&str, Option<&str>, Option<&str>)> {
    let (input, _) = tag("ln")(input)?;
    let (input, currency) = alt((tag("ckb"), tag("ckt")))(input)?;
    let (input, amount) = opt(take_while1(|c: char| c.is_numeric()))(input)?;
    let (input, si) = opt(take_while1(|c: char| ['m', 'u', 'k'].contains(&c)))(input)?;
    Ok((input, (currency, amount, si)))
}

fn parse_hrp(input: &str) -> Result<(Currency, Option<u64>, Option<SiPrefix>), InvoiceParseError> {
    match nom_scan_hrp(input) {
        Ok((left, (currency, amount, si_prefix))) => {
            if !left.is_empty() {
                return Err(InvoiceParseError::MalformedHRP);
            }
            let currency =
                Currency::from_str(currency).map_err(|_| InvoiceParseError::UnknownCurrency)?;
            let amount = amount
                .map(|x| {
                    x.parse()
                        .map_err(|err| InvoiceParseError::ParseAmountError(err))
                })
                .transpose()?;
            let si_prefix = si_prefix
                .map(|x| SiPrefix::from_str(x).map_err(|_| InvoiceParseError::UnknownSiPrefix))
                .transpose()?;
            Ok((currency, amount, si_prefix))
        }
        Err(_) => Err(InvoiceParseError::MalformedHRP),
    }
}

impl From<Attribute> for InvoiceAttr {
    fn from(attr: Attribute) -> Self {
        let a = match attr {
            Attribute::ExpiryTime(x) => {
                let seconds = x.as_secs();
                let nanos = x.subsec_nanos() as u64;
                let value = gen_invoice::Duration::new_builder()
                    .seconds(seconds.pack())
                    .nanos(nanos.pack())
                    .build();
                InvoiceAttrUnion::ExpiryTime(ExpiryTime::new_builder().value(value).build())
            }
            Attribute::Description(value) => InvoiceAttrUnion::Description(
                Description::new_builder().value(value.pack()).build(),
            ),
            Attribute::FinalHtlcTimeout(value) => InvoiceAttrUnion::FinalHtlcTimeout(
                FinalHtlcTimeout::new_builder().value(value.pack()).build(),
            ),
            Attribute::FinalHtlcMinimumCltvExpiry(value) => {
                InvoiceAttrUnion::FinalHtlcMinimumCltvExpiry(
                    FinalHtlcMinimumCltvExpiry::new_builder()
                        .value(value.pack())
                        .build(),
                )
            }
            Attribute::FallbackAddr(value) => InvoiceAttrUnion::FallbackAddr(
                FallbackAddr::new_builder().value(value.pack()).build(),
            ),
            Attribute::Feature(value) => {
                InvoiceAttrUnion::Feature(Feature::new_builder().value(value.pack()).build())
            }
            Attribute::UdtScript(script) => {
                InvoiceAttrUnion::UdtScript(UdtScript::new_builder().value(script).build())
            }
            Attribute::PayeePublicKey(pubkey) => InvoiceAttrUnion::PayeePublicKey(
                PayeePublicKey::new_builder()
                    .value(pubkey.serialize().pack())
                    .build(),
            ),
        };
        InvoiceAttr::new_builder().set(a).build()
    }
}

impl From<InvoiceAttr> for Attribute {
    fn from(attr: InvoiceAttr) -> Self {
        match attr.to_enum() {
            InvoiceAttrUnion::Description(x) => {
                let value: Vec<u8> = x.value().unpack();
                Attribute::Description(String::from_utf8(value).unwrap())
            }
            InvoiceAttrUnion::ExpiryTime(x) => {
                let seconds: u64 = x.value().seconds().unpack();
                let nanos: u64 = x.value().nanos().unpack();
                Attribute::ExpiryTime(
                    Duration::from_secs(seconds).saturating_add(Duration::from_nanos(nanos)),
                )
            }
            InvoiceAttrUnion::FinalHtlcTimeout(x) => {
                Attribute::FinalHtlcTimeout(x.value().unpack())
            }
            InvoiceAttrUnion::FinalHtlcMinimumCltvExpiry(x) => {
                Attribute::FinalHtlcMinimumCltvExpiry(x.value().unpack())
            }
            InvoiceAttrUnion::FallbackAddr(x) => {
                let value: Vec<u8> = x.value().unpack();
                Attribute::FallbackAddr(String::from_utf8(value).unwrap())
            }
            InvoiceAttrUnion::Feature(x) => Attribute::Feature(x.value().unpack()),
            InvoiceAttrUnion::UdtScript(x) => Attribute::UdtScript(x.value()),
            InvoiceAttrUnion::PayeePublicKey(x) => {
                let value: Vec<u8> = x.value().unpack();
                Attribute::PayeePublicKey(PublicKey::from_slice(&value).unwrap())
            }
        }
    }
}

/// Errors that may occur when constructing a new [`RawBolt11Invoice`] or [`Bolt11Invoice`]
#[derive(Eq, PartialEq, Debug, Clone)]
pub enum CreationError {
    /// Duplicated attribute key
    DuplicatedAttributeKey(String),

    /// No payment hash
    NoPaymentHash,

    /// No payment secret
    NoPaymentSecret,
}

pub struct InvoiceBuilder {
    currency: Currency,
    amount: Option<u64>,
    prefix: Option<SiPrefix>,
    payment_hash: Option<[u8; 32]>,
    payment_secret: Option<[u8; 32]>,
    attrs: Vec<Attribute>,
}

impl InvoiceBuilder {
    pub fn new() -> Self {
        Self {
            currency: Currency::Ckb,
            amount: None,
            prefix: None,
            payment_hash: None,
            payment_secret: None,
            attrs: Vec::new(),
        }
    }

    pub fn currency(mut self, currency: Currency) -> Self {
        self.currency = currency;
        self
    }

    pub fn amount(mut self, amount: Option<u64>) -> Self {
        self.amount = amount;
        self
    }

    pub fn prefix(mut self, prefix: Option<SiPrefix>) -> Self {
        self.prefix = prefix;
        self
    }

    pub fn payment_hash(mut self, payment_hash: [u8; 32]) -> Self {
        self.payment_hash = Some(payment_hash);
        self
    }

    pub fn payment_secret(mut self, payment_secret: [u8; 32]) -> Self {
        self.payment_secret = Some(payment_secret);
        self
    }

    pub fn add_attr(mut self, attr: Attribute) -> Self {
        self.attrs.push(attr);
        self
    }

    /// Sets the payee's public key.
    pub fn payee_pub_key(self, pub_key: PublicKey) -> Self {
        self.add_attr(Attribute::PayeePublicKey(pub_key.into()))
    }

    /// Sets the expiry time, dropping the subsecond part (which is not representable in BOLT 11
    /// invoices).
    pub fn expiry_time(self, expiry_time: Duration) -> Self {
        self.add_attr(Attribute::ExpiryTime(expiry_time))
    }

    /// Adds a fallback address.
    pub fn fallback(self, fallback: String) -> Self {
        self.add_attr(Attribute::FallbackAddr(fallback))
    }

    pub fn build(self) -> Result<CkbInvoice, SignOrCreationError> {
        let convert_err = |e| SignOrCreationError::CreationError(e);

        self.check_duplicated_attrs().map_err(convert_err)?;
        Ok(CkbInvoice {
            currency: self.currency,
            amount: self.amount,
            prefix: self.prefix,
            signature: None,
            data: InvoiceData {
                payment_hash: self
                    .payment_hash
                    .ok_or(CreationError::NoPaymentHash)
                    .map_err(convert_err)?,

                payment_secret: self
                    .payment_secret
                    .ok_or(CreationError::NoPaymentSecret)
                    .map_err(convert_err)?,
                attrs: self.attrs,
            },
        })
    }

    pub fn build_with_sign<F>(self, sign_function: F) -> Result<CkbInvoice, SignOrCreationError>
    where
        F: FnOnce(&Message) -> RecoverableSignature,
    {
        let mut invoice = self.build()?;
        invoice.build_signature(sign_function)?;
        Ok(invoice)
    }

    fn check_duplicated_attrs(&self) -> Result<(), CreationError> {
        // check is there any duplicate attribute key set
        for (i, attr) in self.attrs.iter().enumerate() {
            for other in self.attrs.iter().skip(i + 1) {
                if std::mem::discriminant(attr) == std::mem::discriminant(other) {
                    return Err(CreationError::DuplicatedAttributeKey(format!("{:?}", attr)));
                }
            }
        }
        Ok(())
    }
}

/// When signing using a fallible method either an user-supplied `SignError` or a [`CreationError`]
/// may occur.
#[derive(Eq, PartialEq, Debug, Clone)]
pub enum SignOrCreationError {
    /// An error occurred during signing
    SignError,

    /// An error occurred while building the transaction
    CreationError(CreationError),
}

#[derive(Error, Debug)]
pub enum Error {
    #[error("Molecule error: {0}")]
    Molecule(#[from] molecule::error::VerificationError),
}
impl TryFrom<gen_invoice::RawCkbInvoice> for CkbInvoice {
    type Error = Error;

    fn try_from(invoice: gen_invoice::RawCkbInvoice) -> Result<Self, Self::Error> {
        Ok(CkbInvoice {
            currency: (u8::from(invoice.currency())).into(),
            amount: invoice.amount().to_opt().map(|x| x.unpack()),
            prefix: invoice.prefix().to_opt().map(|x| u8::from(x).into()),
            signature: invoice.signature().to_opt().map(|x| {
                let vec_u8: Vec<u8> = x.as_bytes().into();
                let vec_u5: Vec<u5> = vec_u8
                    .iter()
                    .map(|x| u5::try_from_u8(*x).unwrap())
                    .collect();
                InvoiceSignature::from_base32(&vec_u5).unwrap()
            }),
            data: invoice.data().try_into()?,
        })
    }
}

impl From<CkbInvoice> for RawCkbInvoice {
    fn from(invoice: CkbInvoice) -> Self {
        RawCkbInvoiceBuilder::default()
            .currency((invoice.currency as u8).into())
            .amount(
                AmountOpt::new_builder()
                    .set(invoice.amount.map(|x| x.pack()))
                    .build(),
            )
            .prefix(
                SiPrefixOpt::new_builder()
                    .set(invoice.prefix.map(|x| (x as u8).into()))
                    .build(),
            )
            .signature(
                SignatureOpt::new_builder()
                    .set({
                        invoice.signature.map(|x| {
                            let bytes: [u8; 104] = x
                                .to_base32()
                                .iter()
                                .map(|x| x.to_u8())
                                .collect::<Vec<_>>()
                                .as_slice()
                                .try_into()
                                .unwrap();
                            Signature::from(bytes)
                        })
                    })
                    .build(),
            )
            .data(InvoiceData::from(invoice.data).into())
            .build()
    }
}

impl From<InvoiceData> for gen_invoice::RawInvoiceData {
    fn from(data: InvoiceData) -> Self {
        RawInvoiceDataBuilder::default()
            .payment_hash(PaymentHash::from(data.payment_hash))
            .payment_secret(PaymentSecret::from(data.payment_secret))
            .attrs(
                InvoiceAttrsVec::new_builder()
                    .set(
                        data.attrs
                            .iter()
                            .map(|a| a.to_owned().into())
                            .collect::<Vec<InvoiceAttr>>(),
                    )
                    .build(),
            )
            .build()
    }
}

impl TryFrom<gen_invoice::RawInvoiceData> for InvoiceData {
    type Error = Error;

    fn try_from(data: gen_invoice::RawInvoiceData) -> Result<Self, Self::Error> {
        Ok(InvoiceData {
            payment_hash: data.payment_hash().into(),
            payment_secret: data.payment_secret().into(),
            attrs: data
                .attrs()
                .into_iter()
                .map(|a| a.into())
                .collect::<Vec<Attribute>>(),
        })
    }
}

#[cfg(test)]
mod tests {
    use bitcoin::{
        key::{KeyPair, Secp256k1},
        secp256k1::SecretKey,
    };

    use super::*;

    fn random_u8_array(num: usize) -> Vec<u8> {
        (0..num).map(|_| rand::random::<u8>()).collect()
    }

    fn gen_rand_public_key() -> PublicKey {
        let secp = Secp256k1::new();
        let key_pair = KeyPair::new(&secp, &mut rand::thread_rng());
        PublicKey::from_keypair(&key_pair)
    }

    fn gen_rand_private_key() -> SecretKey {
        let secp = Secp256k1::new();
        let key_pair = KeyPair::new(&secp, &mut rand::thread_rng());
        SecretKey::from_keypair(&key_pair)
    }

    fn mock_invoice_no_sign() -> CkbInvoice {
        CkbInvoice {
            currency: Currency::Ckb,
            amount: Some(1280),
            prefix: Some(SiPrefix::Kilo),
            signature: None,
            data: InvoiceData {
                payment_hash: random_u8_array(32).try_into().unwrap(),
                payment_secret: random_u8_array(32).try_into().unwrap(),
                attrs: vec![
                    Attribute::FinalHtlcTimeout(5),
                    Attribute::FinalHtlcMinimumCltvExpiry(12),
                    Attribute::Description("description".to_string()),
                    Attribute::ExpiryTime(Duration::from_secs(1024)),
                    Attribute::FallbackAddr("address".to_string()),
                    Attribute::UdtScript(Script::default()),
                    Attribute::PayeePublicKey(gen_rand_public_key()),
                ],
            },
        }
    }

    fn mock_invoice() -> CkbInvoice {
        let private_key = gen_rand_private_key();
        let mut invoice = mock_invoice_no_sign();
        let _ = invoice
            .build_signature(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key));
        invoice
    }

    #[test]
    fn test_parse_hrp() {
        let res = parse_hrp("lnckb1280k");
        assert_eq!(res, Ok((Currency::Ckb, Some(1280), Some(SiPrefix::Kilo))));

        let res = parse_hrp("lnckb");
        assert_eq!(res, Ok((Currency::Ckb, None, None)));

        let res = parse_hrp("lnckt1023");
        assert_eq!(res, Ok((Currency::CkbTestNet, Some(1023), None)));

        let res = parse_hrp("lnckt1023u");
        assert_eq!(
            res,
            Ok((Currency::CkbTestNet, Some(1023), Some(SiPrefix::Micro)))
        );

        let res = parse_hrp("lncktk");
        assert_eq!(res, Ok((Currency::CkbTestNet, None, Some(SiPrefix::Kilo))));

        let res = parse_hrp("xnckb");
        assert_eq!(res, Err(InvoiceParseError::MalformedHRP));

        let res = parse_hrp("lxckb");
        assert_eq!(res, Err(InvoiceParseError::MalformedHRP));

        let res = parse_hrp("lnckt");
        assert_eq!(res, Ok((Currency::CkbTestNet, None, None)));

        let res = parse_hrp("lnxkt");
        assert_eq!(res, Err(InvoiceParseError::MalformedHRP));

        let res = parse_hrp("lncktt");
        assert_eq!(res, Err(InvoiceParseError::MalformedHRP));

        let res = parse_hrp("lnckt1x24");
        assert_eq!(res, Err(InvoiceParseError::MalformedHRP));

        let res = parse_hrp("lnckt000k");
        assert_eq!(
            res,
            Ok((Currency::CkbTestNet, Some(0), Some(SiPrefix::Kilo)))
        );

        let res =
            parse_hrp("lnckt1024444444444444444444444444444444444444444444444444444444444444");
        assert!(matches!(res, Err(InvoiceParseError::ParseAmountError(_))));

        let res = parse_hrp("lnckt0x");
        assert_eq!(res, Err(InvoiceParseError::MalformedHRP));

        let res = parse_hrp("");
        assert_eq!(res, Err(InvoiceParseError::MalformedHRP));
    }

    #[test]
    fn test_signature() {
        let private_key = gen_rand_private_key();
        let signature = Secp256k1::new()
            .sign_ecdsa_recoverable(&Message::from_slice(&[0u8; 32]).unwrap(), &private_key);
        let signature = InvoiceSignature(signature);
        let base32 = signature.to_base32();
        assert_eq!(base32.len(), 104);

        let decoded_signature = InvoiceSignature::from_base32(&base32).unwrap();
        assert_eq!(decoded_signature, signature);
    }

    #[test]
    fn test_ckb_invoice() {
        let ckb_invoice = mock_invoice();
        let ckb_invoice_clone = ckb_invoice.clone();
        let raw_invoice: RawCkbInvoice = ckb_invoice.into();
        let decoded_invoice: CkbInvoice = raw_invoice.try_into().unwrap();
        assert_eq!(decoded_invoice, ckb_invoice_clone);
        let address = ckb_invoice_clone.to_string();
        assert!(address.starts_with("lnckb1280k1"));
    }

    #[test]
    fn test_invoice_bc32m() {
        let invoice = mock_invoice();

        let address = invoice.to_string();
        assert!(address.starts_with("lnckb1280k1"));

        let decoded_invoice = address.parse::<CkbInvoice>().unwrap();
        assert_eq!(decoded_invoice, invoice);
        assert_eq!(decoded_invoice.is_signed(), true);
    }

    #[test]
    fn test_invoice_bc32m_not_same() {
        let private_key = gen_rand_private_key();
        let signature = Secp256k1::new()
            .sign_ecdsa_recoverable(&Message::from_slice(&[0u8; 32]).unwrap(), &private_key);
        let invoice = CkbInvoice {
            currency: Currency::Ckb,
            amount: Some(1280),
            prefix: Some(SiPrefix::Kilo),
            signature: Some(InvoiceSignature(signature)),
            data: InvoiceData {
                payment_hash: [0u8; 32],
                payment_secret: [0u8; 32],
                attrs: vec![
                    Attribute::FinalHtlcTimeout(5),
                    Attribute::FinalHtlcMinimumCltvExpiry(12),
                    Attribute::Description("description hello".to_string()),
                    Attribute::ExpiryTime(Duration::from_secs(1024)),
                    Attribute::FallbackAddr("address".to_string()),
                ],
            },
        };

        let address = invoice.to_string();
        let decoded_invoice = address.parse::<CkbInvoice>().unwrap();
        assert_eq!(decoded_invoice, invoice);

        let mock_invoice = mock_invoice();
        let mock_address = mock_invoice.to_string();
        assert_ne!(mock_address, address);
    }

    #[test]
    fn test_compress() {
        let input = "hrp1gyqsqqq5qqqqq9gqqqqp6qqqqq0qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq2qqqqqqqqqqqyvqsqqqsqqqqqvqqqqq8";
        let bytes = input.as_bytes();
        let compressed = ar_encompress(input.as_bytes()).unwrap();

        let decompressed = ar_decompress(&compressed).unwrap();
        let decompressed_str = std::str::from_utf8(&decompressed).unwrap();
        assert_eq!(input, decompressed_str);
        assert!(compressed.len() < bytes.len());
    }

    #[test]
    fn test_invoice_builder() {
        let gen_payment_hash = random_u8_array(32).try_into().unwrap();
        let gen_payment_secret = random_u8_array(32).try_into().unwrap();
        let private_key = gen_rand_private_key();

        let invoice = InvoiceBuilder::new()
            .currency(Currency::Ckb)
            .amount(Some(1280))
            .prefix(Some(SiPrefix::Kilo))
            .payment_hash(gen_payment_hash)
            .payment_secret(gen_payment_secret)
            .fallback("address".to_string())
            .expiry_time(Duration::from_secs(1024))
            .payee_pub_key(gen_rand_public_key())
            .add_attr(Attribute::FinalHtlcTimeout(5))
            .add_attr(Attribute::FinalHtlcMinimumCltvExpiry(12))
            .add_attr(Attribute::Description("description".to_string()))
            .add_attr(Attribute::UdtScript(Script::default()))
            .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key))
            .unwrap();

        let address = invoice.to_string();

        assert_eq!(invoice, address.parse::<CkbInvoice>().unwrap());

        assert_eq!(invoice.currency, Currency::Ckb);
        assert_eq!(invoice.amount, Some(1280));
        assert_eq!(invoice.prefix, Some(SiPrefix::Kilo));
        assert_eq!(invoice.data.payment_hash, gen_payment_hash);
        assert_eq!(invoice.data.payment_secret, gen_payment_secret);
        assert_eq!(invoice.data.payment_hash, gen_payment_hash);
        assert_eq!(invoice.data.payment_secret, gen_payment_secret);
        assert_eq!(invoice.data.attrs.len(), 7);
    }

    #[test]
    fn test_invoice_builder_duplicated_attr() {
        let gen_payment_hash = random_u8_array(32).try_into().unwrap();
        let gen_payment_secret = random_u8_array(32).try_into().unwrap();
        let private_key = gen_rand_private_key();
        let invoice = InvoiceBuilder::new()
            .currency(Currency::Ckb)
            .amount(Some(1280))
            .prefix(Some(SiPrefix::Kilo))
            .payment_hash(gen_payment_hash)
            .payment_secret(gen_payment_secret)
            .add_attr(Attribute::FinalHtlcTimeout(5))
            .add_attr(Attribute::FinalHtlcTimeout(6))
            .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key));

        assert_eq!(
            invoice.err(),
            Some(SignOrCreationError::CreationError(
                CreationError::DuplicatedAttributeKey(format!(
                    "{:?}",
                    Attribute::FinalHtlcTimeout(5)
                ),),
            ),)
        );
    }

    #[test]
    fn test_invoice_builder_missing() {
        let private_key = gen_rand_private_key();
        let invoice = InvoiceBuilder::new()
            .currency(Currency::Ckb)
            .amount(Some(1280))
            .prefix(Some(SiPrefix::Kilo))
            .payment_secret(random_u8_array(32).try_into().unwrap())
            .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key));

        assert_eq!(
            invoice.err(),
            Some(SignOrCreationError::CreationError(
                CreationError::NoPaymentHash
            ))
        );

        let invoice = InvoiceBuilder::new()
            .currency(Currency::Ckb)
            .amount(Some(1280))
            .prefix(Some(SiPrefix::Kilo))
            .payment_hash(random_u8_array(32).try_into().unwrap())
            .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key));

        assert_eq!(
            invoice.err(),
            Some(SignOrCreationError::CreationError(
                CreationError::NoPaymentSecret
            ))
        );
    }
}
