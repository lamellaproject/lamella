//! Reads the one field the store's index needs from a certificate: its subject Name, as the raw
//! bytes the certificate carries -- identifier, length and contents -- which are the bytes a TLS
//! library compares with a child certificate's issuer Name. Nothing is decoded and re-encoded: a
//! re-encoded Name can differ from the certificate's own bytes in a string type, the order of a
//! set or the form of a length, and a byte comparison with an issuer Name then misses.
//!
//! Only the structure in front of the subject is walked (RFC 5280, section 4.1): the Certificate
//! SEQUENCE and its TBSCertificate, then the optional `[0]` version, the serial number, the
//! signature algorithm, the issuer and the validity. The rest of the TBSCertificate is not read.

const SEQUENCE: u8 = 0x30;
const INTEGER: u8 = 0x02;
const BIT_STRING: u8 = 0x03;
/// `[0] EXPLICIT`: context-specific, constructed, number 0 -- the TBSCertificate's version.
const VERSION: u8 = 0xa0;

/// One element of a DER encoding (ITU-T X.690): its identifier octet, its whole encoding, and its
/// contents.
pub(crate) struct Element<'a> {
    pub(crate) tag: u8,
    pub(crate) encoding: &'a [u8],
    pub(crate) contents: &'a [u8],
}

/// The element at the start of `input`, and the bytes after it.
pub(crate) fn read_element(input: &[u8]) -> Result<(Element<'_>, &[u8]), String> {
    let [tag, first, rest @ ..] = input else {
        return Err("truncated: an element needs an identifier and a length".to_string());
    };
    if tag & 0x1f == 0x1f {
        return Err(format!(
            "identifier 0x{tag:02x}: a multi-octet tag, which no field read here has"
        ));
    }
    let (length, rest) = if *first < 0x80 {
        (usize::from(*first), rest)
    } else {
        let octets = usize::from(first & 0x7f);
        if octets == 0 {
            return Err("an indefinite length, which DER does not allow".to_string());
        }
        if octets > 4 {
            return Err(format!("a length of {octets} octets"));
        }
        let Some((length, rest)) = rest.split_at_checked(octets) else {
            return Err("truncated inside a length".to_string());
        };
        (
            length
                .iter()
                .fold(0, |length, &octet| length << 8 | usize::from(octet)),
            rest,
        )
    };
    let Some((contents, after)) = rest.split_at_checked(length) else {
        return Err(format!(
            "truncated: the element holds {length} bytes and {} follow its length",
            rest.len()
        ));
    };
    let header = input.len() - rest.len();
    Ok((
        Element {
            tag: *tag,
            encoding: &input[..header + length],
            contents,
        },
        after,
    ))
}

/// The raw subject Name of `certificate`, which must be exactly one Certificate: a SEQUENCE of the
/// TBSCertificate, the signature algorithm and the signature, with nothing after it.
pub(crate) fn subject(certificate: &[u8]) -> Result<&[u8], String> {
    let (certificate, after) = expect(certificate, SEQUENCE, "Certificate")?;
    if !after.is_empty() {
        return Err(format!("{} bytes follow the Certificate", after.len()));
    }
    let (tbs, signature_parts) = expect(certificate.contents, SEQUENCE, "tbsCertificate")?;
    let (_, signature_parts) = expect(signature_parts, SEQUENCE, "signatureAlgorithm")?;
    let (_, rest) = expect(signature_parts, BIT_STRING, "signatureValue")?;
    if !rest.is_empty() {
        return Err(format!("{} bytes follow the signatureValue", rest.len()));
    }

    let mut fields = tbs.contents;
    if fields.first() == Some(&VERSION) {
        fields = read_element(fields)
            .map_err(|error| format!("version: {error}"))?
            .1;
    }
    let (_, fields) = expect(fields, INTEGER, "serialNumber")?;
    let (_, fields) = expect(fields, SEQUENCE, "signature")?;
    let (_, fields) = expect(fields, SEQUENCE, "issuer")?;
    let (_, fields) = expect(fields, SEQUENCE, "validity")?;
    let (subject, _) = expect(fields, SEQUENCE, "subject")?;
    Ok(subject.encoding)
}

/// The element at the start of `input`, which must carry `tag`; `field` names it in an error.
fn expect<'a>(input: &'a [u8], tag: u8, field: &str) -> Result<(Element<'a>, &'a [u8]), String> {
    let (element, after) = read_element(input).map_err(|error| format!("{field}: {error}"))?;
    if element.tag != tag {
        return Err(format!(
            "{field}: identifier 0x{:02x} where 0x{tag:02x} belongs",
            element.tag
        ));
    }
    Ok((element, after))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One DER element with a minimal length, in the short form or the long form as its size needs.
    fn tlv(tag: u8, contents: &[u8]) -> Vec<u8> {
        let mut encoding = vec![tag];
        let length = contents.len();
        if length < 0x80 {
            encoding.push(length as u8);
        } else {
            let octets: Vec<u8> = length
                .to_be_bytes()
                .into_iter()
                .skip_while(|&octet| octet == 0)
                .collect();
            encoding.push(0x80 | octets.len() as u8);
            encoding.extend_from_slice(&octets);
        }
        encoding.extend_from_slice(contents);
        encoding
    }

    /// A Name with one RDN holding one commonName attribute.
    fn name(common_name: &str) -> Vec<u8> {
        let attribute = [
            tlv(0x06, &[0x55, 0x04, 0x03]),
            tlv(0x0c, common_name.as_bytes()),
        ]
        .concat();
        tlv(SEQUENCE, &tlv(0x31, &tlv(SEQUENCE, &attribute)))
    }

    /// A certificate of the right shape; `version` is false for a version 1 certificate, which
    /// leaves the `[0]` field out.
    fn certificate(version: bool, issuer: &[u8], subject: &[u8]) -> Vec<u8> {
        let mut tbs = Vec::new();
        if version {
            tbs.extend(tlv(VERSION, &tlv(INTEGER, &[2])));
        }
        tbs.extend(tlv(INTEGER, &[0x01, 0x23]));
        tbs.extend(tlv(SEQUENCE, &tlv(0x06, &[0x2a, 0x86, 0x48])));
        tbs.extend_from_slice(issuer);
        tbs.extend(tlv(
            SEQUENCE,
            &[tlv(0x17, b"260101000000Z"), tlv(0x17, b"360101000000Z")].concat(),
        ));
        tbs.extend_from_slice(subject);
        tbs.extend(tlv(SEQUENCE, &[0u8; 300]));
        let signature_algorithm = tlv(SEQUENCE, &tlv(0x06, &[0x2a, 0x86, 0x48]));
        tlv(
            SEQUENCE,
            &[
                tlv(SEQUENCE, &tbs),
                signature_algorithm,
                tlv(BIT_STRING, &[0, 1, 2, 3]),
            ]
            .concat(),
        )
    }

    #[test]
    fn the_subject_is_the_raw_name_after_the_validity() {
        let issuer = name("Issuing Authority");
        let subject_name = name("Root Authority");
        for version in [true, false] {
            let der = certificate(version, &issuer, &subject_name);
            assert!(
                der.len() > 0x100,
                "the test certificate exercises a two-octet length"
            );
            assert_eq!(
                subject(&der).unwrap(),
                subject_name,
                "with version: {version}"
            );
        }
    }

    #[test]
    fn a_name_in_the_long_length_form_is_copied_whole() {
        let long_name = name(&"x".repeat(200));
        assert_eq!(long_name[1], 0x81, "the name's length takes the long form");
        let der = certificate(true, &name("Issuer"), &long_name);
        assert_eq!(subject(&der).unwrap(), long_name);
    }

    #[test]
    fn what_is_not_one_certificate_is_refused() {
        let der = certificate(true, &name("Issuer"), &name("Subject"));
        let trailing = [der.as_slice(), &[0x00]].concat();
        assert!(
            subject(&trailing)
                .unwrap_err()
                .contains("follow the Certificate")
        );
        assert!(
            subject(&der[..der.len() - 1])
                .unwrap_err()
                .starts_with("Certificate: truncated")
        );
        assert!(
            subject(&[0x30, 0x80, 0x00, 0x00])
                .unwrap_err()
                .contains("indefinite")
        );
        assert!(
            subject(&[0x30, 0x85, 1, 2, 3, 4, 5])
                .unwrap_err()
                .contains("5 octets")
        );

        let no_subject = tlv(
            SEQUENCE,
            &[
                tlv(SEQUENCE, &[tlv(INTEGER, &[1]), tlv(SEQUENCE, &[])].concat()),
                tlv(SEQUENCE, &[]),
                tlv(BIT_STRING, &[0]),
            ]
            .concat(),
        );
        assert!(
            subject(&no_subject)
                .unwrap_err()
                .starts_with("issuer: truncated")
        );

        let integer_subject = certificate(true, &name("Issuer"), &tlv(INTEGER, &[7]));
        assert_eq!(
            subject(&integer_subject).unwrap_err(),
            "subject: identifier 0x02 where 0x30 belongs"
        );
    }
}
