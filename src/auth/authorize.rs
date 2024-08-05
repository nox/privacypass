//! This module contains the authorization logic for redemption phase of the
//! protocol.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::{engine::general_purpose::URL_SAFE, Engine as _};
use generic_array::{ArrayLength, GenericArray};
use http::{header::HeaderName, HeaderValue};
use nom::branch::alt;
use nom::sequence::delimited;
use nom::{
    bytes::complete::{tag, tag_no_case},
    multi::{many1, separated_list1},
    IResult,
};
use std::io::Write;
use thiserror::Error;
use tls_codec::{Deserialize, Error, Serialize, Size};

use crate::{ChallengeDigest, KeyId, Nonce, TokenType};

use super::{base64_char, key_name, opt_spaces, space};

/// A Token as defined in The Privacy Pass HTTP Authentication Scheme:
///
/// ```text
/// struct {
///     uint16_t token_type = 0x0001
///     uint8_t nonce[32];
///     uint8_t challenge_digest[32];
///     uint8_t token_key_id[32];
///     uint8_t authenticator[Nk];
/// } Token;
/// ```

#[derive(Clone, Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub struct Token<Nk: ArrayLength<u8>> {
    token_type: TokenType,
    nonce: Nonce,
    challenge_digest: ChallengeDigest,
    token_key_id: KeyId,
    authenticator: GenericArray<u8, Nk>,
}

impl<Nk: ArrayLength<u8>> Size for Token<Nk> {
    fn tls_serialized_len(&self) -> usize {
        self.token_type.tls_serialized_len()
            + self.nonce.tls_serialized_len()
            + self.challenge_digest.tls_serialized_len()
            + self.token_key_id.tls_serialized_len()
            + Nk::to_usize()
    }
}

impl<Nk: ArrayLength<u8>> Serialize for Token<Nk> {
    fn tls_serialize<W: Write>(&self, writer: &mut W) -> Result<usize, Error> {
        Ok(self.token_type.tls_serialize(writer)?
            + self.nonce.tls_serialize(writer)?
            + self.challenge_digest.tls_serialize(writer)?
            + self.token_key_id.tls_serialize(writer)?
            + writer.write(&self.authenticator[..])?)
    }
}

impl<Nk: ArrayLength<u8>> Deserialize for Token<Nk> {
    fn tls_deserialize<R: std::io::Read>(bytes: &mut R) -> Result<Self, Error>
    where
        Self: Sized,
    {
        let token_type = TokenType::tls_deserialize(bytes)?;
        let nonce = Nonce::tls_deserialize(bytes)?;
        let challenge_digest = ChallengeDigest::tls_deserialize(bytes)?;
        let token_key_id = KeyId::tls_deserialize(bytes)?;
        let mut authenticator = vec![0u8; Nk::to_usize()];
        let len = bytes.read(authenticator.as_mut_slice())?;
        if len != Nk::to_usize() {
            return Err(Error::InvalidVectorLength);
        }
        Ok(Self {
            token_type,
            nonce,
            challenge_digest,
            token_key_id,
            authenticator: GenericArray::clone_from_slice(&authenticator),
        })
    }
}

impl<Nk: ArrayLength<u8>> Token<Nk> {
    /// Creates a new Token.
    pub const fn new(
        token_type: TokenType,
        nonce: Nonce,
        challenge_digest: ChallengeDigest,
        token_key_id: KeyId,
        authenticator: GenericArray<u8, Nk>,
    ) -> Self {
        Self {
            token_type,
            nonce,
            challenge_digest,
            token_key_id,
            authenticator,
        }
    }

    /// Returns the token type.
    pub const fn token_type(&self) -> TokenType {
        self.token_type
    }

    /// Returns the nonce.
    pub const fn nonce(&self) -> Nonce {
        self.nonce
    }

    /// Returns the challenge digest.
    pub const fn challenge_digest(&self) -> &ChallengeDigest {
        &self.challenge_digest
    }

    /// Returns the token key ID.
    pub const fn token_key_id(&self) -> &KeyId {
        &self.token_key_id
    }

    /// Returns the authenticator.
    pub fn authenticator(&self) -> &[u8] {
        self.authenticator.as_ref()
    }
}

/// Builds a `Authorize` header according to the following scheme:
///
/// `PrivateToken token=...`
///
/// # Errors
/// Returns an error if the token is not valid.
pub fn build_authorization_header<Nk: ArrayLength<u8>>(
    token: &Token<Nk>,
) -> Result<(HeaderName, HeaderValue), BuildError> {
    let value = format!(
        "PrivateToken token={}",
        URL_SAFE.encode(
            token
                .tls_serialize_detached()
                .map_err(|_| BuildError::InvalidToken)?
        ),
    );
    let header_name = http::header::AUTHORIZATION;
    let header_value = HeaderValue::from_str(&value).map_err(|_| BuildError::InvalidToken)?;
    Ok((header_name, header_value))
}

/// Builds a `Authorize` header according to the following scheme:
///
/// `PrivateToken token="...", extensions="..."`
///
/// # Errors
/// Returns an error if the token is not valid.
pub fn build_authorization_header_ext<Nk: ArrayLength<u8>>(
    token: &Token<Nk>,
    extensions: &[u8],
) -> Result<(HeaderName, HeaderValue), BuildError> {
    let value = format!(
        "PrivateToken token=\"{}\", extensions=\"{}\"",
        URL_SAFE.encode(
            token
                .tls_serialize_detached()
                .map_err(|_| BuildError::InvalidToken)?
        ),
        URL_SAFE.encode(extensions),
    );
    let header_name = http::header::AUTHORIZATION;
    let header_value = HeaderValue::from_str(&value).map_err(|_| BuildError::InvalidToken)?;
    Ok((header_name, header_value))
}

/// Building error for the `Authorization` header values
#[derive(Error, Debug)]
pub enum BuildError {
    #[error("Invalid token")]
    /// Invalid token
    InvalidToken,
}

/// Parses an `Authorization` header according to the following scheme:
///
/// `PrivateToken token=...`
///
/// # Errors
/// Returns an error if the header value is not valid.
pub fn parse_authorization_header<Nk: ArrayLength<u8>>(
    value: &HeaderValue,
) -> Result<Token<Nk>, ParseError> {
    let s = value.to_str().map_err(|_| ParseError::InvalidInput)?;
    let tokens = parse_header_value(s)?;
    let token = tokens[0].0.clone();
    Ok(token)
}

/// Parses an `Authorization` header according to the following scheme:
///
/// `PrivateToken token=... [extensions=...]`
///
/// # Errors
/// Returns an error if the header value is not valid.
pub fn parse_authorization_header_ext<Nk: ArrayLength<u8>>(
    value: &HeaderValue,
) -> Result<(Token<Nk>, Option<Vec<u8>>), ParseError> {
    let s = value.to_str().map_err(|_| ParseError::InvalidInput)?;
    let mut tokens = parse_header_value(s)?;
    Ok(tokens.pop().unwrap())
}

/// Parsing error for the `WWW-Authenticate` header values
#[derive(Error, Debug)]
pub enum ParseError {
    #[error("Invalid token")]
    /// Invalid token
    InvalidToken,
    #[error("Invalid input string")]
    /// Invalid input string
    InvalidInput,
}

fn parse_key_value(input: &str) -> IResult<&str, (&str, &str)> {
    let (input, _) = opt_spaces(input)?;
    let (input, key) = key_name(input)?;
    let (input, _) = opt_spaces(input)?;
    let (input, _) = tag("=")(input)?;
    let (input, _) = opt_spaces(input)?;
    let (input, value) = match key.to_lowercase().as_str() {
        "token" | "extensions" => {
            // Values may or may not be delimited with quotes.
            alt((delimited(tag("\""), base64_char, tag("\"")), base64_char))(input)?
        }
        _ => {
            return Err(nom::Err::Failure(nom::error::make_error(
                input,
                nom::error::ErrorKind::Tag,
            )))
        }
    };
    Ok((input, (key, value)))
}

fn parse_private_token(input: &str) -> IResult<&str, (&str, Option<&str>)> {
    let (input, _) = opt_spaces(input)?;
    let (input, _) = tag_no_case("PrivateToken")(input)?;
    let (input, _) = many1(space)(input)?;
    let (input, key_values) = separated_list1(
        alt((tag(","), tag(" "))), // quirk: key=values separated by " " are not RFC 9110 compliant.
        parse_key_value,
    )(input)?;

    let mut token = None;
    let mut extensions = None;
    let err = nom::Err::Failure(nom::error::make_error(input, nom::error::ErrorKind::Tag));

    for (key, value) in key_values {
        match key.to_lowercase().as_str() {
            "token" => {
                if token.is_some() {
                    return Err(err);
                }
                token = Some(value)
            }
            "extensions" => {
                if extensions.is_some() {
                    return Err(err);
                }
                extensions = Some(value)
            }
            _ => return Err(err),
        }
    }
    let token = token.ok_or(err)?;

    Ok((input, (token, extensions)))
}

fn parse_private_tokens(input: &str) -> IResult<&str, Vec<(&str, Option<&str>)>> {
    separated_list1(tag(","), parse_private_token)(input)
}

fn parse_header_value<Nk: ArrayLength<u8>>(
    input: &str,
) -> Result<Vec<(Token<Nk>, Option<Vec<u8>>)>, ParseError> {
    let (output, tokens) = parse_private_tokens(input).map_err(|_| ParseError::InvalidInput)?;
    if !output.is_empty() {
        return Err(ParseError::InvalidInput);
    }
    let tokens = tokens
        .into_iter()
        .map(|(token_value, extensions_value)| {
            let ext = extensions_value.and_then(|x| {
                URL_SAFE_NO_PAD
                    .decode(x)
                    .or_else(|_| URL_SAFE.decode(x))
                    .ok()
            });
            Ok((
                Token::tls_deserialize(
                    &mut URL_SAFE
                        .decode(token_value)
                        .map_err(|_| ParseError::InvalidToken)?
                        .as_slice(),
                )
                .map_err(|_| ParseError::InvalidToken)?,
                ext,
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use generic_array::GenericArray;
    use http::HeaderValue;
    use typenum::U32;

    use crate::auth::authorize::{
        build_authorization_header, build_authorization_header_ext, parse_authorization_header,
        parse_authorization_header_ext, Token,
    };
    use crate::TokenType;

    #[test]
    fn builder_parser_test() {
        use generic_array::typenum::U32;

        let token = test_token();
        let (header_name, header_value) = build_authorization_header(&token).unwrap();

        assert_eq!(header_name, http::header::AUTHORIZATION);

        let token = parse_authorization_header::<U32>(&header_value).unwrap();
        assert_eq!(token, test_token());
    }

    #[test]
    fn builder_parser_extensions_test() {
        use generic_array::typenum::U32;

        let token = test_token();
        let extensions = b"hello world";
        let (header_name, header_value) =
            build_authorization_header_ext(&token, extensions).unwrap();

        assert_eq!(header_name, http::header::AUTHORIZATION);

        let (token, maybe_extensions) =
            parse_authorization_header_ext::<U32>(&header_value).unwrap();
        assert_eq!(token, test_token());
        assert_eq!(maybe_extensions, Some(extensions.to_vec()));
    }

    /// This is the same test as `builder_parser_extensions_test`, however we
    /// replace the `, ` separator with a ` ` (single space) to make sure the
    /// library can handle tokens with the older header format.
    ///
    /// This and the associated quirk is to be deleted when all clients have
    /// upgraded.
    #[test]
    fn rfc_9110_regression_test() {
        use generic_array::typenum::U32;

        let token = test_token();
        let extensions = b"hello world";
        let (header_name, header_value) =
            build_authorization_header_ext(&token, extensions).unwrap();

        // remove the separating comma from the generated header:
        let header_value =
            HeaderValue::from_str(&header_value.to_str().unwrap().replace(", ", " ")).unwrap();

        assert_eq!(header_name, http::header::AUTHORIZATION);

        let (token, maybe_extensions) =
            parse_authorization_header_ext::<U32>(&header_value).unwrap();
        assert_eq!(token, test_token());
        assert_eq!(maybe_extensions, Some(extensions.to_vec()));
    }

    fn test_token() -> Token<U32> {
        let nonce = [1u8; 32];
        let challenge_digest = [2u8; 32];
        let token_key_id = [3u8; 32];
        let authenticator = [4u8; 32];
        Token::<U32>::new(
            TokenType::PrivateToken,
            nonce,
            challenge_digest,
            token_key_id,
            GenericArray::clone_from_slice(&authenticator),
        )
    }
}
