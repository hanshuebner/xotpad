use libxotpad::x25::packet::X25CallRequest;
use libxotpad::x3::X3ParamError;
use std::fmt::{self, Write};

/// X.28 _service_ signal.
pub enum X28Signal {
    Connected(Option<X25CallRequest>),
    Cleared(Option<(u8, u8)>),
    Free,
    Engaged,
    LocalParams(Vec<(u8, Result<u8, X3ParamError>)>),
    RemoteParams(Vec<(u8, Result<u8, X3ParamError>)>),
    Error,
}

impl fmt::Display for X28Signal {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            X28Signal::Connected(Some(_)) => write!(fmt, "COM"),
            X28Signal::Connected(None) => write!(fmt, "COM"),
            X28Signal::Cleared(Some((cause_code, diagnostic_code))) => {
                let cause = clear_cause(*cause_code);

                write!(fmt, "CLR {cause} C:{cause_code} D:{diagnostic_code}")
            }
            X28Signal::Cleared(None) => write!(fmt, "CLR CONF"),
            X28Signal::Free => write!(fmt, "FREE"),
            X28Signal::Engaged => write!(fmt, "ENGAGED"),
            X28Signal::LocalParams(params) => {
                let params = format_params(params);

                write!(fmt, "PAR {params}")
            }
            X28Signal::RemoteParams(params) => {
                let params = format_params(params);

                write!(fmt, "RPAR {params}")
            }
            X28Signal::Error => write!(fmt, "ERR"),
        }
    }
}

fn clear_cause(_code: u8) -> &'static str {
    "TODO"
}

fn format_params(params: &[(u8, Result<u8, X3ParamError>)]) -> String {
    let mut s = String::new();

    for (param, value) in params {
        if !s.is_empty() {
            s.push_str(", ");
        }

        let _ = match value {
            Ok(value) => write!(&mut s, "{param}:{value}"),
            Err(_) => write!(&mut s, "{param}:INV"),
        };
    }

    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_cleared_local() {
        let signal = X28Signal::Cleared(None);

        assert_eq!(signal.to_string(), "CLR CONF");
    }

    #[test]
    fn fmt_cleared_remote() {
        let signal = X28Signal::Cleared(Some((0, 0)));

        assert_eq!(signal.to_string(), "CLR TODO C:0 D:0");
    }

    #[test]
    fn fmt_local_params() {
        let signal = X28Signal::LocalParams(vec![
            (1, Ok(1)),
            (2, Err(X3ParamError::Unsupported)),
            (3, Ok(3)),
        ]);

        assert_eq!(signal.to_string(), "PAR 1:1, 2:INV, 3:3");
    }

    #[test]
    fn fmt_remote_params() {
        let signal = X28Signal::RemoteParams(vec![
            (1, Ok(1)),
            (2, Err(X3ParamError::Unsupported)),
            (3, Ok(3)),
        ]);

        assert_eq!(signal.to_string(), "RPAR 1:1, 2:INV, 3:3");
    }
}
