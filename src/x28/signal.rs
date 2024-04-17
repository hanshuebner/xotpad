use std::fmt::Write;

pub fn format_params(params: &[(u8, Option<u8>)]) -> String {
    let mut s = String::new();

    for &(param, value) in params {
        if !s.is_empty() {
            s.push_str(", ");
        }

        match value {
            Some(value) => write!(&mut s, "{param}:{value}"),
            None => write!(&mut s, "{param}:INV"),
        };
    }

    s
}

#[cfg(test)]
mod tests {
    #[test]
    fn format_params() {
        let params = [(1, Some(1)), (2, None), (3, Some(3))];

        assert_eq!(super::format_params(&params), "1:1, 2:INV, 3:3");
    }
}
