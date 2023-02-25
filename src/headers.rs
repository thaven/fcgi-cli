use std::collections::HashMap;

use nom::{
    branch::alt,
    bytes::complete::{tag, take_while, take_while1},
    character::complete::{space0, line_ending},
    combinator::{recognize, map},
    error::{make_error, ErrorKind, Error},
    IResult,
    multi::{fold_many1, many0},
    sequence::{separated_pair, terminated, delimited}, Finish
};

pub fn parse_headers(input: &[u8]) -> Result<(&[u8], HashMap<String, String>), Error<&[u8]>> {
    terminated(
        fold_many1(
            generic_field,
            HashMap::new,
            |mut acc: HashMap<String, String>, kv: (&[u8], &[u8])| {
                // We expect headers to be in ASCII, so let's prevent unnecessary
                // UTF-8 decoding. However, we do not check whether all bytes are
                // actually valid ASCII, instead we assume ISO-8859-1 (latin1)
                // encoding, which is a superset of ASCII and a subset of Unicode.
                acc.insert(
                    latin1_to_string(kv.0)
                        .to_ascii_lowercase(),
                    latin1_to_string(kv.1)
                );
                acc
            }
        ),
        line_ending
    )(input).finish()
}

fn generic_field(input: &[u8]) -> IResult<&[u8], (&[u8], &[u8])> {
    terminated(
        separated_pair(
            token,
            terminated(tag(b":"), space0),
            field_content //field_value
        ),
        line_ending
    )(input)
}

fn token(input: &[u8]) -> IResult<&[u8], &[u8]> {
    take_while1(|b: u8| {
        !b"()<>@,;:\\\"/[]?={} ".contains(&b)
            && !(b as char).is_ascii_control()
    })(input)
}

fn separator(input: &[u8]) -> IResult<&[u8], &u8> {
    if input.len() > 0 && b"()<>@,;:\\\"/[]?={} \t".contains(&input[0]) {
        Ok((&input[1..], &input[0]))
    } else {
        // Probably this is not the way to do it, but it does the job for now.
        Err(nom::Err::Error(make_error(input, ErrorKind::IsA)))
    }
}

fn quoted_string(input: &[u8]) -> IResult<&[u8], &[u8]> {
    delimited(
        tag("\""),
        take_while(|b: u8| {
            b != b'"' && (b == b'\t' || !(b as char).is_ascii_control())
        }),
        tag("\"")
    )(input)
}

fn field_content(input: &[u8]) -> IResult<&[u8], &[u8]> {
    // CGI/1.1 spec includes NL in LWSP, and allows LWSP in headers....
    // Further down it states that headers must be single line, which
    // seems contradictory. Also, correctly parsing a header containing
    // NL seems impossible, so let's assume LWSP should not include NL
    // at all.
    //
    // And then it follows that we do not need to state LWSP explicitly,
    // as the remaining characters HT and SP are separators.

    recognize(
        many0(
            alt((
                token,
                map(separator, std::slice::from_ref),
                quoted_string
            ))
        )
    )(input)
}

pub fn latin1_to_string(s: &[u8]) -> String {
    s.iter().map(|&c| c as char).collect()
}