use std::str::FromStr;

use crate::x28::selection::X28Selection;

/// X.28 command _signal_.
#[derive(PartialEq, Debug)]
pub enum X28Command {
    Selection(X28Selection),
    Clear,
    Read(Vec<u8>),
    Set(Vec<(u8, u8)>),
    SetRead(Vec<(u8, u8)>),
    Status,
    InviteClear,
    Help(String),
}

impl FromStr for X28Command {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let pair: Vec<&str> = s.trim().splitn(2, ' ').collect();

        let command = pair[0].to_uppercase();
        let rest = if pair.len() > 1 { pair[1] } else { "" };

        match &command[..] {
            "CALL" => {
                if rest.is_empty() {
                    return Err("selection argument required".into());
                }

                match X28Selection::from_str(rest) {
                    Ok(selection) => Ok(X28Command::Selection(selection)),
                    Err(_) => Err("invalid selection".into()),
                }
            }
            "CLR" | "CLEAR" => Ok(X28Command::Clear),
            "PAR?" | "PAR" | "PARAMETER" | "READ" => {
                let params = parse_read_params(rest)?;

                Ok(X28Command::Read(params))
            }
            "SET" => {
                let params = parse_set_params(rest)?;

                if params.is_empty() {
                    return Err("parameters argument required".into());
                }

                Ok(X28Command::Set(params))
            }
            "SET?" | "SETREAD" => {
                let params = parse_set_params(rest)?;

                if params.is_empty() {
                    return Err("parameters argument required".into());
                }

                Ok(X28Command::SetRead(params))
            }
            "STAT" | "STATUS" => Ok(X28Command::Status),
            "ICLR" | "ICLEAR" => Ok(X28Command::InviteClear),
            "HELP" => Ok(X28Command::Help(rest.to_string())),
            _ => match X28Selection::from_str(&command) {
                Ok(selection) => Ok(X28Command::Selection(selection)),
                _ => Err("unrecognized command".into()),
            },
        }
    }
}

// Cisco and RAD both implement subtly different handling of invalid input, this
// is closer to the RAD implementation which is more straightforward to implement.
fn parse_read_params(s: &str) -> Result<Vec<u8>, String> {
    let s = s.trim();

    if s.is_empty() {
        return Ok(vec![]);
    }

    s.split(',')
        .map(|a| u8::from_str(a.trim()).map_err(|_| "invalid parameter".into()))
        .collect()
}

fn parse_set_params(s: &str) -> Result<Vec<(u8, u8)>, String> {
    let s = s.trim();

    if s.is_empty() {
        return Ok(vec![]);
    }

    s.split(',')
        .map(|a| {
            let Some((param, value)) = a.split_once(':') else {
                return Err("invalid parameters argument".into());
            };

            let Ok(param) = u8::from_str(param.trim()) else {
                return Err("invalid parameter".into());
            };

            let Ok(value) = u8::from_str(value.trim()) else {
                return Err("invalid value".into());
            };

            Ok((param, value))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use libxotpad::x121::X121Addr;

    use crate::x28::X28Addr;

    use super::*;

    #[test]
    fn from_str_selection() {
        let Ok(X28Command::Selection(selection)) = X28Command::from_str("12345") else {
            panic!();
        };

        assert_eq!(
            selection.addrs,
            &[X28Addr::Full(X121Addr::from_str("12345").unwrap())]
        );
    }

    #[test]
    fn from_str_call_selection() {
        let Ok(X28Command::Selection(selection)) = X28Command::from_str("call 12345") else {
            panic!();
        };

        assert_eq!(
            selection.addrs,
            &[X28Addr::Full(X121Addr::from_str("12345").unwrap())]
        );
    }

    #[test]
    fn from_str_call_selection_invalid() {
        assert!(X28Command::from_str("call").is_err());
    }

    #[test]
    fn from_str_clear() {
        assert_eq!(X28Command::from_str("clr"), Ok(X28Command::Clear));
        assert_eq!(X28Command::from_str("clear"), Ok(X28Command::Clear));
    }

    #[test]
    fn from_str_read() {
        assert_eq!(X28Command::from_str("par?"), Ok(X28Command::Read(vec![])));
        assert_eq!(
            X28Command::from_str("par? 1"),
            Ok(X28Command::Read(vec![1]))
        );
        assert_eq!(
            X28Command::from_str("par? 1,2"),
            Ok(X28Command::Read(vec![1, 2]))
        );
        assert_eq!(
            X28Command::from_str("par? 1, 2"),
            Ok(X28Command::Read(vec![1, 2]))
        );
    }

    #[test]
    fn from_str_read_invalid() {
        assert!(X28Command::from_str("par? a").is_err());
        assert!(X28Command::from_str("par? 1,a").is_err());
        assert!(X28Command::from_str("par? ,").is_err());
    }

    #[test]
    fn from_str_set() {
        assert_eq!(
            X28Command::from_str("set 1:1"),
            Ok(X28Command::Set(vec![(1, 1)]))
        );
        assert_eq!(
            X28Command::from_str("set 1:1,2:2"),
            Ok(X28Command::Set(vec![(1, 1), (2, 2)]))
        );
        assert_eq!(
            X28Command::from_str("set 1: 1, 2 : 2"),
            Ok(X28Command::Set(vec![(1, 1), (2, 2)]))
        );
    }

    #[test]
    fn from_str_set_invalid() {
        assert!(X28Command::from_str("set").is_err());
        assert!(X28Command::from_str("set 1").is_err());
        assert!(X28Command::from_str("set 1:a").is_err());
        assert!(X28Command::from_str("set a").is_err());
        assert!(X28Command::from_str("set ,").is_err());
    }

    #[test]
    fn from_str_set_read() {
        assert_eq!(
            X28Command::from_str("set? 1:1"),
            Ok(X28Command::SetRead(vec![(1, 1)]))
        );
        assert_eq!(
            X28Command::from_str("set? 1:1,2:2"),
            Ok(X28Command::SetRead(vec![(1, 1), (2, 2)]))
        );
        assert_eq!(
            X28Command::from_str("set? 1: 1, 2 : 2"),
            Ok(X28Command::SetRead(vec![(1, 1), (2, 2)]))
        );
    }

    #[test]
    fn from_str_set_read_invalid() {
        assert!(X28Command::from_str("set?").is_err());
        assert!(X28Command::from_str("set? 1").is_err());
        assert!(X28Command::from_str("set? 1:a").is_err());
        assert!(X28Command::from_str("set? a").is_err());
        assert!(X28Command::from_str("set? ,").is_err());
    }

    #[test]
    fn from_str_status() {
        assert_eq!(X28Command::from_str("stat"), Ok(X28Command::Status));
        assert_eq!(X28Command::from_str("status"), Ok(X28Command::Status));
    }

    #[test]
    fn from_str_invite_clear() {
        assert_eq!(X28Command::from_str("iclr"), Ok(X28Command::InviteClear));
        assert_eq!(X28Command::from_str("iclear"), Ok(X28Command::InviteClear));
    }

    #[test]
    fn from_str_help() {
        assert_eq!(
            X28Command::from_str("help"),
            Ok(X28Command::Help("".to_string()))
        );
        assert_eq!(
            X28Command::from_str("help subject"),
            Ok(X28Command::Help("subject".to_string()))
        );
    }

    #[test]
    fn from_str_invalid() {
        assert!(X28Command::from_str("invalid").is_err());
    }
}
