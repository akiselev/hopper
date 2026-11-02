//! A small XML scanner for the documents the remote protocol transfers: target descriptions
//! (`<reg>`, `<xi:include>`, `<architecture>`) and thread lists (`<thread>`). It yields elements
//! with their attributes and text; it does not validate and ignores comments, processing
//! instructions and DOCTYPE.

use hopper_core::{HopperError, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    /// Text directly inside the element, up to its first child or closing tag.
    pub text: String,
}

impl Element {
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';') else {
            out.push_str(rest);
            return out;
        };
        let ent = &rest[1..end];
        let decoded = match ent {
            "lt" => Some('<'),
            "gt" => Some('>'),
            "amp" => Some('&'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => {
                if let Some(hex) = ent.strip_prefix("#x") {
                    u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
                } else if let Some(dec) = ent.strip_prefix('#') {
                    dec.parse().ok().and_then(char::from_u32)
                } else {
                    None
                }
            }
        };
        match decoded {
            Some(c) => out.push(c),
            None => out.push_str(&rest[..=end]),
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

fn parse_attrs(s: &str) -> Result<Vec<(String, String)>> {
    let mut attrs = Vec::new();
    let mut rest = s.trim();
    while !rest.is_empty() {
        let eq = rest
            .find('=')
            .ok_or_else(|| HopperError::Protocol(format!("xml attribute without '=': {rest:?}")))?;
        let key = rest[..eq].trim().to_string();
        let after = rest[eq + 1..].trim_start();
        let quote = after
            .chars()
            .next()
            .filter(|c| *c == '"' || *c == '\'')
            .ok_or_else(|| HopperError::Protocol(format!("unquoted xml attribute {key:?}")))?;
        let close = after[1..]
            .find(quote)
            .ok_or_else(|| HopperError::Protocol(format!("unterminated xml attribute {key:?}")))?;
        attrs.push((key, decode_entities(&after[1..1 + close])));
        rest = after[close + 2..].trim_start();
    }
    Ok(attrs)
}

/// Every start (or empty) element in document order.
pub fn elements(doc: &str) -> Result<Vec<Element>> {
    let mut out = Vec::new();
    let mut rest = doc;
    while let Some(lt) = rest.find('<') {
        rest = &rest[lt..];
        if let Some(r) = rest.strip_prefix("<!--") {
            let end = r
                .find("-->")
                .ok_or_else(|| HopperError::Protocol("unterminated xml comment".into()))?;
            rest = &r[end + 3..];
            continue;
        }
        let gt = rest
            .find('>')
            .ok_or_else(|| HopperError::Protocol("unterminated xml tag".into()))?;
        let tag = &rest[1..gt];
        rest = &rest[gt + 1..];
        if tag.starts_with('?') || tag.starts_with('!') || tag.starts_with('/') {
            continue;
        }
        let (body, empty) = match tag.strip_suffix('/') {
            Some(b) => (b, true),
            None => (tag, false),
        };
        let (name, attrs) = match body.find(char::is_whitespace) {
            Some(i) => (&body[..i], parse_attrs(&body[i..])?),
            None => (body, Vec::new()),
        };
        let text = if empty {
            String::new()
        } else {
            let end = rest.find('<').unwrap_or(rest.len());
            decode_entities(rest[..end].trim())
        };
        out.push(Element {
            name: name.to_string(),
            attrs,
            text,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_target_description() {
        let doc = r#"<?xml version="1.0"?>
<!DOCTYPE target SYSTEM "gdb-target.dtd">
<!-- a comment with <reg name="fake"/> inside -->
<target>
<architecture>i386:x86-64</architecture>
<xi:include href="64bit-core.xml"/>
  <reg name="rax" bitsize='64' type="int64" regnum="0"/>
  <thread id="p1.2" name="a &amp; b"/>
</target>"#;
        let els = elements(doc).unwrap();
        let names: Vec<&str> = els.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            ["target", "architecture", "xi:include", "reg", "thread"]
        );
        assert_eq!(els[1].text, "i386:x86-64");
        assert_eq!(els[2].attr("href"), Some("64bit-core.xml"));
        assert_eq!(els[3].attr("bitsize"), Some("64"));
        assert_eq!(els[3].attr("regnum"), Some("0"));
        assert_eq!(els[4].attr("name"), Some("a & b"));
    }

    #[test]
    fn rejects_broken_attributes() {
        assert!(elements("<reg name=rax/>").is_err());
        assert!(elements("<reg name=\"rax/>").is_err());
        assert!(elements("<reg").is_err());
    }
}
