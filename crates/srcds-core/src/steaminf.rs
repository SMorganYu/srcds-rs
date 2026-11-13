//! Minimal parser for `steam.inf` (`key=value` lines), used to report the same
//! `PatchVersion` as the installed game files.

use std::collections::HashMap;

pub fn parse(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|l| {
            let (k, v) = l.split_once('=')?;
            Some((k.trim().to_string(), v.trim().to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_gmod_steam_inf() {
        let m = super::parse("PatchVersion=2026.04.29\r\nProductName=garrysmod\r\nappID=4000\r\n");
        assert_eq!(m["PatchVersion"], "2026.04.29");
        assert_eq!(m["appID"], "4000");
    }
}
