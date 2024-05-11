use libxotpad::x121::X121Addr;
use std::str::FromStr;

/// X.28 selection.
#[derive(Clone, PartialEq, Debug)]
pub struct X28Selection {
    pub addrs: Vec<X28Addr>,
    pub facilities: Vec<(char, String)>,
    pub call_user_data: String,
}

/// X.28 address.
#[derive(Clone, PartialEq, Debug)]
pub enum X28Addr {
    Full(X121Addr),
    Abbrev(String),
}

impl FromStr for X28Selection {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        if s.is_empty() {
            return Err("empty selection".into());
        }

        let (facilities, rest) = parse_facilities(s)?;

        let (addrs, call_user_data) = parse_addrs(rest)?;

        // Clone, for now...
        Ok(X28Selection {
            addrs,
            facilities: facilities
                .iter()
                .map(|&(f, v)| (f, v.to_string()))
                .collect(),
            call_user_data: call_user_data.to_string(),
        })
    }
}

#[allow(clippy::type_complexity)] // TODO: X28Facility enum
fn parse_facilities(s: &str) -> Result<(Vec<(char, &str)>, &str), String> {
    // Special case to allow for a selection like ".abbrev-with-hyphen".
    if s.starts_with('.') {
        return Ok((vec![], s));
    }

    if !s.contains('-') {
        return Ok((vec![], s));
    }

    let mut facilities = vec![];

    let mut rest = s;

    while !rest.is_empty() {
        let index = rest.find(|c| c == ',' || c == '-').unwrap_or(rest.len());

        let facility;

        (facility, rest) = rest.split_at(index);

        if !facility.is_empty() {
            facilities.push(parse_facility(facility)?);
        }

        if !rest.starts_with(',') {
            break;
        }

        rest = &rest[1..];
    }

    Ok((facilities, &rest[1..]))
}

fn parse_addrs(s: &str) -> Result<(Vec<X28Addr>, &str), String> {
    let mut addrs = vec![];
    let mut has_abbrev_addrs = false;

    let mut rest = s;

    while !rest.is_empty() {
        let mut is_abbrev = false;

        if rest.starts_with('.') {
            is_abbrev = true;

            has_abbrev_addrs = true;
        }

        let index = rest
            .find(|c| c == ',' || c == '*' || (!has_abbrev_addrs && (c == 'P' || c == 'D')))
            .unwrap_or(rest.len());

        let mut addr;

        (addr, rest) = rest.split_at(index);

        if is_abbrev {
            addr = &addr[1..];
        }

        if !addr.is_empty() {
            let addr = if is_abbrev {
                X28Addr::Abbrev(addr.to_string())
            } else {
                X28Addr::Full(X121Addr::from_str(addr)?)
            };

            addrs.push(addr);
        }

        if !rest.starts_with(',') {
            break;
        }

        rest = &rest[1..];
    }

    if addrs.is_empty() {
        return Err("at least one address is required".to_string());
    }

    if !rest.is_empty() {
        #[allow(clippy::nonminimal_bool)]
        if (has_abbrev_addrs && !rest.starts_with('*'))
            || (!has_abbrev_addrs
                && !(rest.starts_with('*') || rest.starts_with('P') || rest.starts_with('D')))
        {
            return Err("invalid call user data separator".to_string());
        }

        rest = &rest[1..];
    }

    Ok((addrs, rest))
}

#[allow(clippy::type_complexity)] // TODO: X28Facility enum
fn parse_facility(s: &str) -> Result<(char, &str), String> {
    assert!(!s.is_empty());

    Ok((s.chars().next().unwrap(), &s[1..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_str() {
        assert_from_str(
            "R,Ncisc-.abcPD,12345,.info*cud",
            &[
                abbrev_addr("abcPD"),
                full_addr("12345"),
                abbrev_addr("info"),
            ],
            &[('R', ""), ('N', "cisc")],
            "cud",
        );

        assert_from_str("12345", &[full_addr("12345")], &[], "");

        assert_from_str("-12345", &[full_addr("12345")], &[], "");
        assert_from_str("R-12345", &[full_addr("12345")], &[('R', "")], "");
        assert_from_str("Ncisc-12345", &[full_addr("12345")], &[('N', "cisc")], "");
        assert_from_str(
            "R,Ncisc-12345",
            &[full_addr("12345")],
            &[('R', ""), ('N', "cisc")],
            "",
        );

        assert_from_str("12345*cud", &[full_addr("12345")], &[], "cud");
        assert_from_str("12345Pcud", &[full_addr("12345")], &[], "cud");
        assert_from_str("12345Dcud", &[full_addr("12345")], &[], "cud");

        assert_from_str(".abcPD", &[abbrev_addr("abcPD")], &[], "");
        assert_from_str(".999", &[abbrev_addr("999")], &[], "");

        assert_from_str(".abcPD*cud", &[abbrev_addr("abcPD")], &[], "cud");
        assert_from_str(".999*cud", &[abbrev_addr("999")], &[], "cud");

        assert_from_str(
            "12345,6789",
            &[full_addr("12345"), full_addr("6789")],
            &[],
            "",
        );
        assert_from_str(
            "12345,6789*cud",
            &[full_addr("12345"), full_addr("6789")],
            &[],
            "cud",
        );
        assert_from_str(
            "12345,6789Pcud",
            &[full_addr("12345"), full_addr("6789")],
            &[],
            "cud",
        );
        assert_from_str(
            "12345,6789Dcud",
            &[full_addr("12345"), full_addr("6789")],
            &[],
            "cud",
        );
        assert_from_str(
            "12345,.abcPD",
            &[full_addr("12345"), abbrev_addr("abcPD")],
            &[],
            "",
        );
        assert_from_str(
            "12345,.abcPD,.9",
            &[full_addr("12345"), abbrev_addr("abcPD"), abbrev_addr("9")],
            &[],
            "",
        );
        assert_from_str(
            "12345,.abcPD,.9*cud",
            &[full_addr("12345"), abbrev_addr("abcPD"), abbrev_addr("9")],
            &[],
            "cud",
        );

        assert_from_str("-.1-2-3.com", &[abbrev_addr("1-2-3.com")], &[], "");
        assert_from_str(
            "R-.1-2-3.com",
            &[abbrev_addr("1-2-3.com")],
            &[('R', "")],
            "",
        );
    }

    #[test]
    fn from_str_invalid() {
        assert!(X28Selection::from_str("").is_err());
        assert!(X28Selection::from_str("-").is_err());
        assert!(X28Selection::from_str("*").is_err());
        assert!(X28Selection::from_str("-*").is_err());

        assert!(X28Selection::from_str("test").is_err());
    }

    fn assert_from_str(
        s: &str,
        expected_addrs: &[X28Addr],
        expected_facilities: &[(char, &str)],
        expected_call_user_data: &str,
    ) {
        let Ok(selection) = X28Selection::from_str(s) else {
            panic!();
        };

        assert_eq!(selection.addrs, expected_addrs);

        let expected_facilities = expected_facilities
            .iter()
            .map(|&(f, v)| (f, v.to_string()))
            .collect::<Vec<_>>();

        assert_eq!(selection.facilities, expected_facilities);
        assert_eq!(selection.call_user_data, expected_call_user_data);
    }

    fn full_addr(s: &str) -> X28Addr {
        X28Addr::Full(X121Addr::from_str(s).unwrap())
    }

    fn abbrev_addr(s: &str) -> X28Addr {
        X28Addr::Abbrev(s.to_string())
    }
}
